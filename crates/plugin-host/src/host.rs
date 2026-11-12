//! Installing plugins.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use dispatch::{RegistryError, ToolRegistry};
use plugin_api::{
    Capabilities, ManifestError, Plugin, PluginCapability, PluginError, PluginManifest, Tool,
};
use store::{Store, StoreError};

use crate::guarded::{CapabilityMeter, CapabilitySource, GuardedTool};
use crate::service::{self, ServiceError, ServiceSpec};
use crate::{PluginsConfig, ScopedRecords};

/// A plugin could not be installed. Nothing it declared was registered.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HostError {
    /// `plugins.yaml` can't be used.
    #[error("plugins config: {0}")]
    Config(String),
    /// The manifest can't be used.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// The name is already installed.
    #[error("plugin {0} is already installed")]
    Duplicate(String),
    /// It asked for something the deployment didn't grant.
    #[error("plugin {plugin} needs {capability:?}, which plugins.yaml doesn't grant")]
    NotGranted {
        /// The plugin.
        plugin: String,
        /// What it asked for.
        capability: PluginCapability,
    },
    /// It asked for something this build can't provide yet.
    #[error("plugin {plugin} needs {capability:?}, which isn't available in this build")]
    NotAvailable {
        /// The plugin.
        plugin: String,
        /// What it asked for.
        capability: PluginCapability,
    },
    /// Its tools aren't the ones its manifest declares.
    #[error("plugin {plugin}: {problem}")]
    ToolMismatch {
        /// The plugin.
        plugin: String,
        /// What differs.
        problem: String,
    },
    /// It failed to set itself up.
    #[error("plugin {plugin}: {source}")]
    Setup {
        /// The plugin.
        plugin: String,
        /// What went wrong.
        source: PluginError,
    },
    /// Its record types were refused.
    #[error("plugin {plugin}: {source}")]
    Records {
        /// The plugin.
        plugin: String,
        /// What the store said.
        source: StoreError,
    },
    /// A tool was refused by the registry.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// Its process couldn't be started or misbehaved.
    #[error("plugin {plugin}: {source}")]
    Service {
        /// The plugin.
        plugin: String,
        /// What went wrong.
        source: ServiceError,
    },
}

/// Collects plugins, then hands over the finished tool registry.
#[derive(Debug)]
pub struct PluginHost {
    store: Arc<dyn Store>,
    config: PluginsConfig,
    registry: ToolRegistry,
    installed: BTreeSet<String>,
    source: Option<Arc<dyn CapabilitySource>>,
}

/// What a plugin is handed, and how its tools must be guarded.
struct Granted {
    capabilities: Capabilities,
    /// Set when it can call a model: its calls' cost is charged from here.
    meter: Option<Arc<CapabilityMeter>>,
    /// Whether it can bring in outside text, so its output is untrusted.
    guarded: bool,
}

fn compare_tools(manifest: &PluginManifest, tools: &[Arc<dyn Tool>]) -> Result<(), HostError> {
    let mismatch = |problem: String| HostError::ToolMismatch {
        plugin: manifest.name.clone(),
        problem,
    };
    let declared: BTreeMap<String, _> = manifest
        .definitions()
        .into_iter()
        .map(|definition| (definition.name.clone(), definition))
        .collect();
    if tools.len() != declared.len() {
        return Err(mismatch(format!(
            "it declares {} tools and built {}",
            declared.len(),
            tools.len()
        )));
    }
    for tool in tools {
        let built = tool.definition();
        match declared.get(&built.name) {
            None => return Err(mismatch(format!("{} isn't declared", built.name))),
            Some(definition) if *definition != built => {
                return Err(mismatch(format!(
                    "{} differs from what the manifest declares",
                    built.name
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Wraps the tools of a plugin that can bring in outside text, so what they
/// return is untrusted and what their model calls cost is charged.
fn guard(tools: Vec<Arc<dyn Tool>>, granted: &Granted) -> Vec<Arc<dyn Tool>> {
    if !granted.guarded {
        return tools;
    }
    tools
        .into_iter()
        .map(|inner| {
            Arc::new(GuardedTool {
                inner,
                meter: granted.meter.clone(),
            }) as Arc<dyn Tool>
        })
        .collect()
}

impl PluginHost {
    /// A host with no plugins, which is a complete runtime.
    #[must_use]
    pub fn new(store: Arc<dyn Store>, config: PluginsConfig, registry: ToolRegistry) -> Self {
        Self {
            store,
            config,
            registry,
            installed: BTreeSet::new(),
            source: None,
        }
    }

    /// Where the `http` and `model` capabilities come from. Without one, a
    /// plugin that asks for either is refused as not available.
    #[must_use]
    pub fn with_capabilities(mut self, source: Arc<dyn CapabilitySource>) -> Self {
        self.source = Some(source);
        self
    }

    fn claim_name(&self, manifest: &PluginManifest) -> Result<(), HostError> {
        if self.installed.contains(&manifest.name) {
            return Err(HostError::Duplicate(manifest.name.clone()));
        }
        Ok(())
    }

    /// Builds the capabilities a plugin declared, if each was granted.
    fn capabilities(&self, manifest: &PluginManifest) -> Result<Granted, HostError> {
        let mut capabilities = Capabilities::default();
        let mut meter = None;
        let not_available = |capability: &PluginCapability| HostError::NotAvailable {
            plugin: manifest.name.clone(),
            capability: *capability,
        };
        for capability in &manifest.capabilities {
            if !self.config.granted(&manifest.name, *capability) {
                return Err(HostError::NotGranted {
                    plugin: manifest.name.clone(),
                    capability: *capability,
                });
            }
            match capability {
                PluginCapability::Records => {
                    capabilities.records = Some(Arc::new(ScopedRecords::new(
                        Arc::clone(&self.store),
                        &manifest.name,
                    )));
                }
                PluginCapability::Http => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or_else(|| not_available(capability))?;
                    let meter = Arc::clone(meter.get_or_insert_with(Arc::default));
                    capabilities.http = Some(source.http(&manifest.name, meter));
                }
                PluginCapability::Model => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or_else(|| not_available(capability))?;
                    let meter = Arc::clone(meter.get_or_insert_with(Arc::default));
                    capabilities.model = Some(source.model(&manifest.name, meter));
                }
                _ => return Err(not_available(capability)),
            }
        }
        let guarded = capabilities.http.is_some() || capabilities.model.is_some();
        Ok(Granted {
            capabilities,
            meter,
            guarded,
        })
    }

    fn register_tools(
        &mut self,
        manifest: &PluginManifest,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<(), HostError> {
        for tool in tools {
            self.registry.register(tool)?;
        }
        self.installed.insert(manifest.name.clone());
        Ok(())
    }

    /// Installs a plugin compiled into this binary.
    ///
    /// # Errors
    ///
    /// [`HostError`] saying why it was refused.
    pub async fn install_compiled(&mut self, plugin: &dyn Plugin) -> Result<(), HostError> {
        let manifest = PluginManifest::from_yaml(plugin.manifest())?;
        self.claim_name(&manifest)?;
        let granted = self.capabilities(&manifest)?;
        let tools = plugin
            .tools(&granted.capabilities)
            .map_err(|source| HostError::Setup {
                plugin: manifest.name.clone(),
                source,
            })?;
        compare_tools(&manifest, &tools)?;
        let tools = guard(tools, &granted);
        self.declare_records(&manifest).await?;
        self.register_tools(&manifest, tools)
    }

    async fn declare_records(&self, manifest: &PluginManifest) -> Result<(), HostError> {
        if let Some(records) = &manifest.records {
            self.store
                .register_records(records)
                .await
                .map_err(|source| HostError::Records {
                    plugin: manifest.name.clone(),
                    source,
                })?;
        }
        Ok(())
    }

    /// Starts a plugin as its own process and installs its tools. It gets
    /// the capabilities its manifest declares and `plugins.yaml` grants, as
    /// calls back over its connection, only the environment variables its
    /// spec names, and everything it returns is labelled untrusted.
    ///
    /// # Errors
    ///
    /// [`HostError`] saying why it was refused.
    pub async fn install_service(
        &mut self,
        manifest_yaml: &str,
        spec: &ServiceSpec,
    ) -> Result<(), HostError> {
        let manifest = PluginManifest::from_yaml(manifest_yaml)?;
        self.claim_name(&manifest)?;
        let granted = self.capabilities(&manifest)?;
        self.declare_records(&manifest).await?;
        let tools = service::start(&manifest, spec, granted.capabilities.clone())
            .await
            .map_err(|source| HostError::Service {
                plugin: manifest.name.clone(),
                source,
            })?;
        compare_tools(&manifest, &tools)?;
        let tools = guard(tools, &granted);
        self.register_tools(&manifest, tools)
    }

    /// The names installed.
    #[must_use]
    pub fn installed(&self) -> Vec<String> {
        self.installed.iter().cloned().collect()
    }

    /// The finished registry, ready for sessions.
    #[must_use]
    pub fn into_registry(self) -> ToolRegistry {
        self.registry
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dispatch::{Effect, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
    use plugin_api::{CapabilityError, RecordsCapability};
    use protocol::{DataClass, RunId, Trust};
    use serde_json::{Value, json};
    use store::RecordReader;
    use store_memory::MemoryStore;

    const SCHEMA: &str =
        "{type: object, additionalProperties: false, properties: {a: {type: integer}}}";

    fn manifest_text(tail: &str) -> String {
        format!(
            "api_version: 1\nname: demo\nversion: 1.0.0\ndescription: d\ntools:\n  - name: demo_tool\n    description: t\n    input_schema: {SCHEMA}\n    output_schema: {SCHEMA}\n    effect: read\n{tail}"
        )
    }

    fn with_records() -> String {
        manifest_text(&format!(
            "capabilities: [records]\nrecords:\n  types:\n    - name: demo.a\n      schema: {SCHEMA}\n      filterable: []\n  relations: []\n"
        ))
    }

    #[derive(Debug)]
    struct Demo {
        manifest: String,
        effect: Effect,
        name: &'static str,
    }

    impl Demo {
        fn new(manifest: String) -> Self {
            Self {
                manifest,
                effect: Effect::Read,
                name: "demo_tool",
            }
        }
    }

    #[derive(Debug)]
    struct DemoTool(Effect, &'static str);

    fn schema_value() -> Value {
        json!({ "type": "object", "additionalProperties": false,
                "properties": { "a": { "type": "integer" } } })
    }

    #[async_trait]
    impl Tool for DemoTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(
                self.1.to_owned(),
                "t".to_owned(),
                schema_value(),
                schema_value(),
                self.0,
            )
        }

        async fn run(&self, _: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
            Ok(ToolOutput::new(json!({}), Trust::Trusted))
        }
    }

    impl Plugin for Demo {
        fn manifest(&self) -> &str {
            &self.manifest
        }

        fn tools(&self, capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
            let _ = capabilities;
            Ok(vec![Arc::new(DemoTool(self.effect, self.name))])
        }
    }

    fn config(capabilities: &str) -> PluginsConfig {
        PluginsConfig::from_yaml(&format!(
            "version: 1\nplugins:\n  - {{name: demo, capabilities: [{capabilities}]}}\n"
        ))
        .unwrap()
    }

    fn host(capabilities: &str) -> (PluginHost, Arc<MemoryStore>) {
        let store = Arc::new(MemoryStore::new(3));
        let host = PluginHost::new(
            Arc::clone(&store) as Arc<dyn Store>,
            config(capabilities),
            ToolRegistry::new(),
        );
        (host, store)
    }

    fn context() -> ToolContext {
        ToolContext::new(RunId::generate(), DataClass::Public, 0)
    }

    #[tokio::test]
    async fn a_plugin_with_no_needs_installs() {
        let (mut host, _) = host("");
        host.install_compiled(&Demo::new(manifest_text("")))
            .await
            .unwrap();
        assert_eq!(host.installed(), ["demo"]);
    }

    #[tokio::test]
    async fn a_capability_not_granted_stops_the_install() {
        let (mut host, _) = host("");
        let error = host
            .install_compiled(&Demo::new(with_records()))
            .await
            .unwrap_err();
        assert!(matches!(error, HostError::NotGranted { .. }), "{error}");
        assert_eq!(host.installed(), [] as [std::string::String; 0]);
    }

    #[tokio::test]
    async fn a_capability_this_build_lacks_says_so() {
        let (mut host, _) = host("http");
        let text = manifest_text("capabilities: [http]\n");
        let error = host.install_compiled(&Demo::new(text)).await.unwrap_err();
        assert!(matches!(error, HostError::NotAvailable { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_grant_alone_gives_nothing_undeclared() {
        let (mut host, _) = host("records");
        host.install_compiled(&Demo::new(manifest_text("")))
            .await
            .unwrap();
        let manifest = PluginManifest::from_yaml(&manifest_text("")).unwrap();
        let capabilities = host.capabilities(&manifest).unwrap();
        assert!(matches!(
            capabilities.capabilities.records(),
            Err(CapabilityError::NotGranted(PluginCapability::Records))
        ));
    }

    #[tokio::test]
    async fn a_plugin_cannot_install_twice() {
        let (mut host, _) = host("");
        host.install_compiled(&Demo::new(manifest_text("")))
            .await
            .unwrap();
        let error = host
            .install_compiled(&Demo::new(manifest_text("")))
            .await
            .unwrap_err();
        assert!(matches!(error, HostError::Duplicate(_)));
    }

    #[tokio::test]
    async fn a_tool_differing_from_its_manifest_is_refused() {
        let (mut host, _) = host("");
        let mut writes = Demo::new(manifest_text(""));
        writes.effect = Effect::Write;
        let error = host.install_compiled(&writes).await.unwrap_err();
        assert!(matches!(error, HostError::ToolMismatch { .. }), "{error}");
    }

    #[tokio::test]
    async fn an_undeclared_tool_is_refused() {
        let (mut host, _) = host("");
        let mut other = Demo::new(manifest_text(""));
        other.name = "demo_other";
        let error = host.install_compiled(&other).await.unwrap_err();
        assert!(matches!(error, HostError::ToolMismatch { .. }), "{error}");
        assert_eq!(host.installed(), [] as [std::string::String; 0]);
    }

    #[tokio::test]
    async fn a_declared_tool_that_is_not_built_is_refused() {
        let (mut host, _) = host("");
        let text = manifest_text("").replace(
            "    effect: read\n",
            &format!(
                "    effect: read\n  - name: demo_second\n    description: t\n    input_schema: {SCHEMA}\n    output_schema: {SCHEMA}\n    effect: read\n"
            ),
        );
        let error = host.install_compiled(&Demo::new(text)).await.unwrap_err();
        assert!(matches!(error, HostError::ToolMismatch { .. }), "{error}");
        assert_eq!(host.installed(), [] as [std::string::String; 0]);
    }

    #[tokio::test]
    async fn a_service_asking_for_what_isnt_granted_is_refused_before_it_starts() {
        let (mut host, _) = host("");
        let spec = ServiceSpec {
            command: "true".to_owned(),
            args: vec![],
            env_allow: vec![],
            timeout_secs: 5,
        };
        let error = host
            .install_service(&with_records(), &spec)
            .await
            .unwrap_err();
        assert!(matches!(error, HostError::NotGranted { .. }), "{error}");
    }

    #[tokio::test]
    async fn the_records_capability_is_limited_to_the_plugin() {
        let (mut host, store) = host("records");
        host.install_compiled(&Demo::new(with_records()))
            .await
            .unwrap();
        let scoped = ScopedRecords::new(Arc::clone(&store) as Arc<dyn Store>, "demo");
        let id = scoped
            .put("demo.a", None, json!({ "a": 1 }), context())
            .await
            .unwrap();
        let record = scoped.get("demo.a", id, context()).await.unwrap().unwrap();
        assert_eq!(record.provenance.data_class, DataClass::Public);
        assert_eq!(record.provenance.trust, Trust::Untrusted);
        assert_eq!(record.provenance.stored_by.as_str(), "plugin:demo");

        // Another plugin's records exist, and still can't be touched.
        let mut other = PluginManifest::from_yaml(&with_records())
            .unwrap()
            .records
            .unwrap();
        other.types[0].name = "other.a".to_owned();
        store.register_records(&other).await.unwrap();
        let theirs = ScopedRecords::new(Arc::clone(&store) as Arc<dyn Store>, "other");
        let their_id = theirs
            .put("other.a", None, json!({ "a": 2 }), context())
            .await
            .unwrap();

        let refused = |result: Result<(), CapabilityError>| {
            assert!(
                matches!(&result, Err(CapabilityError::Refused(m)) if m.contains("belong")),
                "{result:?}"
            );
        };
        refused(
            scoped
                .put("other.a", None, json!({ "a": 3 }), context())
                .await
                .map(|_| ()),
        );
        refused(scoped.get("other.a", their_id, context()).await.map(|_| ()));
        refused(scoped.delete("other.a", their_id, context()).await);
        // A prefix means the whole name up to the dot.
        refused(scoped.delete("demob.a", id, context()).await);
        assert!(
            theirs
                .get("other.a", their_id, context())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_record_failing_its_schema_is_refused_through_the_capability() {
        let (mut host, store) = host("records");
        host.install_compiled(&Demo::new(with_records()))
            .await
            .unwrap();
        let scoped = ScopedRecords::new(Arc::clone(&store) as Arc<dyn Store>, "demo");
        let error = scoped
            .put("demo.a", None, json!({ "a": "x" }), context())
            .await
            .unwrap_err();
        assert!(matches!(error, CapabilityError::Refused(_)));
    }

    #[test]
    fn plugins_config_refuses_unknown_keys() {
        assert!(PluginsConfig::from_yaml("version: 1\nplugins: []\nextra: 1\n").is_err());
    }

    #[test]
    fn plugins_config_reports_broken_yaml() {
        assert!(PluginsConfig::from_yaml("a: [").is_err());
    }
}
