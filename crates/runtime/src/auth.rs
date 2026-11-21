//! Who's calling. A person has a role, and each of their keys has scopes;
//! a request may do what its key's scopes say, as far as its person's role
//! allows. Both are checked on every request, so lowering a role or
//! revoking a key takes effect at once.
//!
//! People and keys are records the core owns, kept through the store port
//! like Harness²'s, so every store keeps them and an export carries them.
//! No plugin can read them: a plugin reaches only its own records, and
//! `core` is a name no plugin may take. A key's secret is never kept, only
//! its SHA-256: the secret is random, not a password, so a plain hash is
//! enough.
//!
//! The owner isn't a person. The owner key stays in the environment and
//! can't be locked out.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use protocol::{Actor, DataClass, Payer, Trust};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use store::{
    FieldKind, FieldOp, FieldValue, FilterableField, Page, PageRequest, Provenance, Record,
    RecordDecls, RecordId, RecordQuery, RecordTypeDecl, Store, StoreError, WriteOp,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// The record type of a person.
pub(crate) const PERSON_TYPE: &str = "core.person";
/// The record type of a key.
pub(crate) const KEY_TYPE: &str = "core.key";
/// The record type of when a key was last used. It's apart from the key, so
/// noting a use can never undo a revocation made at the same moment.
pub(crate) const KEY_USE_TYPE: &str = "core.key_use";
/// What every key starts with, so a secret scanner can find a leaked one.
pub(crate) const KEY_PREFIX: &str = "exk_";
/// How many random bytes a key's secret has.
const SECRET_BYTES: usize = 32;
/// What a person's slug may be.
pub(crate) const SLUG_PATTERN: &str = "^[a-z0-9][a-z0-9-]{0,47}$";
/// What a person's id is: `person:` and the slug.
pub(crate) const PERSON_PATTERN: &str = "^person:[a-z0-9][a-z0-9-]{0,47}$";
/// A digest, as hex.
const DIGEST_PATTERN: &str = "^[0-9a-f]{64}$";
/// The actor the owner key acts as.
pub(crate) const OWNER: &str = "owner";

/// What a key may be used for.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Read jobs, runs, definitions, Harness² and metrics, and call tools
    /// that only read.
    Read,
    /// Chat, through MCP or the front door, and mark a run good or bad.
    Chat,
    /// Start workflow runs, and call tools that change something.
    Run,
    /// Start Harness² attempts, revert its changes and hold agents.
    Admin,
}

impl Scope {
    /// Its name, as a key lists it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Chat => "chat",
            Self::Run => "run",
            Self::Admin => "admin",
        }
    }
}

/// What a person may do: the most their keys' scopes can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Reads.
    Viewer,
    /// Reads, chats and runs workflows.
    Member,
    /// Everything but people and keys, which are the owner's.
    Admin,
}

impl Role {
    /// Whether a key of a person with this role may use `scope`.
    #[must_use]
    pub const fn allows(self, scope: Scope) -> bool {
        match self {
            Self::Viewer => matches!(scope, Scope::Read),
            Self::Member => !matches!(scope, Scope::Admin),
            Self::Admin => true,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Member => "member",
            Self::Admin => "admin",
        }
    }
}

/// Who a caller is: the owner, or a person with a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallerRole {
    /// A viewer.
    Viewer,
    /// A member.
    Member,
    /// An admin.
    Admin,
    /// The owner, with the owner key.
    Owner,
}

impl From<Role> for CallerRole {
    fn from(role: Role) -> Self {
        match role {
            Role::Viewer => Self::Viewer,
            Role::Member => Self::Member,
            Role::Admin => Self::Admin,
        }
    }
}

/// A person, as kept and as the API shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersonView {
    /// `person:` and their slug. It's the actor on what they start, and is
    /// never reused.
    #[schemars(length(max = 55), regex(pattern = PERSON_PATTERN))]
    pub id: String,
    /// Their name, for showing.
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    /// What they may do.
    pub role: Role,
    /// A disabled person's keys all stop working.
    pub disabled: bool,
    /// When they were added.
    pub made_at: DateTime<Utc>,
    /// Who added them.
    #[schemars(length(max = 64))]
    pub made_by: String,
    /// What they may spend on priced models a UTC day, across all
    /// providers, in millionths of a dollar. Left out, they get the
    /// server's `people.default_day_cap_usd_micros`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day_cap_usd_micros: Option<u64>,
}

/// A person to add, or what to change about one. People are never deleted,
/// so an actor on an old record always resolves; disable one instead.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersonRequest {
    /// Their name, for showing.
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    /// What they may do.
    pub role: Role,
    /// Whether their keys stop working. False if left out.
    #[serde(default)]
    pub disabled: bool,
    /// What they may spend on priced models a UTC day, across all
    /// providers, in millionths of a dollar. Left out, they get the
    /// server's `people.default_day_cap_usd_micros`.
    #[serde(default)]
    pub day_cap_usd_micros: Option<u64>,
}

/// A page of people, by slug.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PersonList {
    /// The people.
    #[schemars(length(max = 200))]
    pub items: Vec<PersonView>,
    /// True if there are more past this page.
    pub has_more: bool,
}

/// A key, as kept. The secret isn't kept, only its digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyRecord {
    #[schemars(length(max = 36))]
    pub(crate) id: String,
    #[schemars(length(max = 55), regex(pattern = PERSON_PATTERN))]
    pub(crate) person: String,
    #[schemars(length(min = 1, max = 4))]
    pub(crate) scopes: Vec<Scope>,
    #[schemars(length(min = 1, max = 128))]
    pub(crate) name: String,
    #[schemars(length(max = 64), regex(pattern = DIGEST_PATTERN))]
    pub(crate) digest: String,
    pub(crate) made_at: DateTime<Utc>,
    pub(crate) revoked_at: Option<DateTime<Utc>>,
    pub(crate) expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(crate) private_by_default: bool,
}

/// When a key was last used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyUse {
    pub(crate) last_used_at: DateTime<Utc>,
}

/// A key, as the API shows it: never its secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct KeyView {
    /// Its id, the part of the key after `exk_` and before the secret.
    #[schemars(length(max = 36))]
    pub id: String,
    /// Whose it is.
    #[schemars(length(max = 55))]
    pub person: String,
    /// What it may be used for, as far as its person's role allows.
    #[schemars(length(max = 4))]
    pub scopes: Vec<Scope>,
    /// What it's for, such as "Claude Code on the desktop".
    #[schemars(length(max = 128))]
    pub name: String,
    /// When it was made.
    pub made_at: DateTime<Utc>,
    /// When it was last used, to the minute. None if it never was.
    pub last_used_at: Option<DateTime<Utc>>,
    /// When it was revoked, if it was.
    pub revoked_at: Option<DateTime<Utc>>,
    /// When it stops working, if it was given an end.
    pub expires_at: Option<DateTime<Utc>>,
    /// True if what it starts is private unless a request says otherwise.
    pub private_by_default: bool,
}

/// A key to make.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyRequest {
    /// Whose it is: `person:` and their slug.
    #[schemars(length(max = 55), regex(pattern = PERSON_PATTERN))]
    pub person: String,
    /// What it may be used for. Each must be one the person's role allows.
    #[schemars(length(min = 1, max = 4))]
    pub scopes: Vec<Scope>,
    /// What it's for, such as "Claude Code on the desktop".
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    /// When it stops working. It never does if left out.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// True if what it starts is private to its person unless a request
    /// says otherwise. False if left out.
    #[serde(default)]
    pub private_by_default: bool,
}

/// A key just made, with its secret. This is the only time the secret is
/// shown.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct KeyMade {
    /// The key.
    pub key: KeyView,
    /// What to send as the bearer token: `exk_`, the id and the secret.
    #[schemars(length(max = 128))]
    pub secret: String,
}

/// A page of keys, oldest first.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct KeyList {
    /// The keys.
    #[schemars(length(max = 200))]
    pub items: Vec<KeyView>,
    /// True if there are more past this page.
    pub has_more: bool,
}

/// Who a request came from, and what its key may do.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CallerView {
    /// The actor on what it starts: `owner`, or a person's id.
    #[schemars(length(max = 64))]
    pub actor: String,
    /// The owner, or the person's role.
    pub role: CallerRole,
    /// What the key may do now: its scopes that the role allows.
    #[schemars(length(max = 4))]
    pub scopes: Vec<Scope>,
    /// The key's id. None for the owner key.
    #[schemars(length(max = 36))]
    pub key: Option<String>,
    /// True if what it starts is private unless a request says otherwise.
    pub private_by_default: bool,
    /// What a person may spend today and has spent. None for the owner,
    /// whom only the providers' caps hold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend: Option<SpendView>,
}

/// A person's spend on priced models today (UTC), across all providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SpendView {
    /// The day, as `YYYY-MM-DD`.
    pub day: chrono::NaiveDate,
    /// What they may spend that day, in millionths of a dollar.
    pub day_cap_usd_micros: u64,
    /// What they've spent or have reserved for calls still running.
    pub spent_usd_micros: u64,
}

/// Who a request came from. The guard puts one on every request it lets
/// through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Caller {
    pub(crate) actor: Actor,
    /// The person's role, or None for the owner.
    pub(crate) role: Option<Role>,
    pub(crate) scopes: BTreeSet<Scope>,
    pub(crate) key: Option<RecordId>,
    /// Whether what it starts is private unless a request says otherwise.
    pub(crate) private_by_default: bool,
}

impl Caller {
    /// The owner key: everything.
    pub(crate) fn owner() -> Self {
        Self {
            actor: Actor::new(OWNER),
            role: None,
            scopes: [Scope::Read, Scope::Chat, Scope::Run, Scope::Admin].into(),
            key: None,
            private_by_default: false,
        }
    }

    /// Whether what this caller starts is private: what the request says,
    /// or the key's default.
    pub(crate) fn private(&self, asked: Option<bool>) -> bool {
        asked.unwrap_or(self.private_by_default)
    }

    /// Whose private runs this caller may see: everyone's for the owner,
    /// otherwise only their own.
    pub(crate) fn visibility(&self) -> events::Visibility {
        if self.is_owner() {
            events::Visibility::All
        } else {
            events::Visibility::Own(self.actor.to_string())
        }
    }

    /// Whether this caller may see a run or job asked for by `requested_by`.
    pub(crate) fn sees(&self, private: bool, requested_by: Option<&str>) -> bool {
        self.visibility().shows(private, requested_by)
    }

    pub(crate) const fn is_owner(&self) -> bool {
        self.role.is_none()
    }

    /// Whether this caller may use `scope`, and if not, why.
    ///
    /// # Errors
    ///
    /// What's missing: the scope on the key, or a role that allows it.
    pub(crate) fn may(&self, scope: Scope) -> Result<(), String> {
        if !self.scopes.contains(&scope) {
            return Err(format!("this needs a key with the {} scope", scope.name()));
        }
        match self.role {
            Some(role) if !role.allows(scope) => Err(format!(
                "this key has the {} scope, but a {} can't use it",
                scope.name(),
                role.name()
            )),
            _ => Ok(()),
        }
    }

    pub(crate) fn view(&self) -> CallerView {
        CallerView {
            actor: self.actor.to_string(),
            role: self.role.map_or(CallerRole::Owner, CallerRole::from),
            scopes: self
                .scopes
                .iter()
                .copied()
                .filter(|scope| self.may(*scope).is_ok())
                .collect(),
            key: self.key.map(|key| key.to_string()),
            private_by_default: self.private_by_default,
            spend: None,
        }
    }
}

/// Who pays for what `requested_by` starts: a person, at their own day cap
/// or the server's default. A disabled or unknown person may spend nothing,
/// and so may one whose record can't be read: a cap fails closed. The owner,
/// schedules and the runtime have no payer.
pub(crate) async fn payer(
    store: &dyn Store,
    default_day_cap_usd_micros: u64,
    requested_by: Option<&str>,
) -> Option<Payer> {
    let actor = requested_by.filter(|actor| actor.starts_with("person:"))?;
    let cap = match person(store, actor).await {
        Ok(Some(found)) if !found.disabled => found
            .day_cap_usd_micros
            .unwrap_or(default_day_cap_usd_micros),
        Ok(_) => 0,
        Err(error) => {
            eprintln!("couldn't read {actor}'s day cap, so they may spend nothing: {error}");
            0
        }
    };
    Some(Payer::new(actor, cap))
}

/// The most people the spend gauges read.
const MAX_PEOPLE_COUNTED: u32 = 1_000;

impl crate::Runtime {
    /// Today's spend and day cap of each priced provider and each person,
    /// for the metrics. What can't be read is left out, and says so.
    pub(crate) async fn spend_today(&self) -> Vec<crate::metrics::DaySpend> {
        let store = self.workflows.store.as_ref();
        let day = Utc::now().date_naive();
        let mut days = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for model in self.gateway.catalog().models() {
            let provider = &model.provider;
            let Some(cap) = provider.daily_cap_usd_micros else {
                continue;
            };
            if !seen.insert(provider.name.clone()) {
                continue;
            }
            match store.spent_on(&provider.name, day).await {
                Ok(spent) => days.push(crate::metrics::DaySpend {
                    account: "provider",
                    name: provider.name.clone(),
                    spent_usd_micros: spent,
                    cap_usd_micros: cap,
                }),
                Err(error) => {
                    eprintln!("metrics: couldn't read {}'s spend: {error}", provider.name);
                }
            }
        }
        let mut offset = 0;
        while offset < MAX_PEOPLE_COUNTED {
            let Ok(page) = PageRequest::new(store::MAX_PAGE_LIMIT, offset) else {
                break;
            };
            let found = match people(store, page).await {
                Ok(found) => found,
                Err(error) => {
                    eprintln!("metrics: couldn't list people: {error}");
                    break;
                }
            };
            for person in &found.items {
                if let Some(payer) = self.payer_for(Some(&person.id)).await {
                    match spend_today(store, payer).await {
                        Ok(spend) => days.push(crate::metrics::DaySpend {
                            account: "person",
                            name: person.id.clone(),
                            spent_usd_micros: spend.spent_usd_micros,
                            cap_usd_micros: spend.day_cap_usd_micros,
                        }),
                        Err(error) => {
                            eprintln!("metrics: couldn't read {}'s spend: {error}", person.id);
                        }
                    }
                }
            }
            if !found.has_more {
                break;
            }
            offset = offset.saturating_add(u32::try_from(found.items.len()).unwrap_or(u32::MAX));
        }
        days
    }

    /// Who pays for what `requested_by` starts, as [`payer`] says.
    pub(crate) async fn payer_for(&self, requested_by: Option<&str>) -> Option<Payer> {
        payer(
            self.workflows.store.as_ref(),
            self.config.people.default_day_cap_usd_micros,
            requested_by,
        )
        .await
    }
}

/// What `payer` has spent today.
///
/// # Errors
///
/// [`StoreError`] if the store fails.
pub(crate) async fn spend_today(store: &dyn Store, payer: Payer) -> Result<SpendView, StoreError> {
    let day = Utc::now().date_naive();
    Ok(SpendView {
        day,
        day_cap_usd_micros: payer.day_cap_usd_micros(),
        spent_usd_micros: store.spent_on(&payer.account(), day).await?,
    })
}

/// Why a key was refused. Each says which, except that a key whose id is
/// unknown and one whose secret is wrong get the same answer, so a guess
/// can't learn which ids exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    Unknown,
    Revoked,
    Expired,
    Disabled,
}

impl Refused {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::Unknown => "send the owner key or a key that exists as a bearer token",
            Self::Revoked => "this key was revoked",
            Self::Expired => "this key has expired",
            Self::Disabled => "this key's person is disabled",
        }
    }
}

/// The record types people and keys are kept as.
pub(crate) fn declarations() -> RecordDecls {
    let declared = |name: &str, schema: Value, filterable: Vec<FilterableField>| RecordTypeDecl {
        name: name.to_owned(),
        schema,
        filterable,
    };
    RecordDecls {
        types: vec![
            declared(PERSON_TYPE, schema::schema_of::<PersonView>(), Vec::new()),
            declared(
                KEY_TYPE,
                schema::schema_of::<KeyRecord>(),
                vec![FilterableField {
                    name: "person".to_owned(),
                    kind: FieldKind::Text,
                }],
            ),
            declared(KEY_USE_TYPE, schema::schema_of::<KeyUse>(), Vec::new()),
        ],
        relations: Vec::new(),
    }
}

/// Whether `slug` can name a person.
pub(crate) fn valid_slug(slug: &str) -> bool {
    slug.len() <= 48
        && slug.bytes().enumerate().all(|(at, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (byte == b'-' && at > 0)
        })
        && !slug.is_empty()
}

/// Whether `id` can be a person's id: `person:` and a slug.
pub(crate) fn valid_person(id: &str) -> bool {
    id.strip_prefix("person:").is_some_and(valid_slug)
}

/// A person's record id, from their id: the same id every time.
pub(crate) fn person_record(person: &str) -> RecordId {
    let seed = format!("{PERSON_TYPE}|{person}");
    RecordId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()))
}

/// Where a key's last use is kept.
fn use_record(key: RecordId) -> RecordId {
    let seed = format!("{KEY_USE_TYPE}|{key}");
    RecordId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn digest_of(secret: &str) -> String {
    hex(&Sha256::digest(secret.as_bytes()))
}

/// A new key's id, and the token to hand out for it.
///
/// # Errors
///
/// Why, if the system has no randomness to give.
pub(crate) fn new_secret() -> Result<(RecordId, String, String), String> {
    let mut bytes = [0_u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    let secret = hex(&bytes);
    let id = RecordId::generate();
    let token = format!("{KEY_PREFIX}{}_{secret}", id.as_uuid().simple());
    Ok((id, token, digest_of(&secret)))
}

/// A token's key id and secret, if it has a key's shape.
fn parse(token: &str) -> Option<(RecordId, &str)> {
    let (id, secret) = token.strip_prefix(KEY_PREFIX)?.split_once('_')?;
    let id = Uuid::try_parse(id).ok()?;
    (secret.len() == SECRET_BYTES * 2).then_some((RecordId::from_uuid(id), secret))
}

/// Whether a token looks like a key rather than the owner key.
pub(crate) fn is_key(token: &str) -> bool {
    token.starts_with(KEY_PREFIX)
}

fn provenance(stored_by: &Actor) -> Provenance {
    Provenance {
        data_class: DataClass::Private,
        trust: Trust::Trusted,
        stored_by: stored_by.clone(),
        stored_at: Utc::now(),
    }
}

fn data_of<T: serde::de::DeserializeOwned>(record: &Record) -> Result<T, StoreError> {
    serde_json::from_value(record.data.clone())
        .map_err(|error| StoreError::Backend(format!("a stored {}: {error}", record.record_type)))
}

fn to_data(value: &impl Serialize) -> Result<Value, StoreError> {
    serde_json::to_value(value).map_err(|error| StoreError::Backend(error.to_string()))
}

/// The person with this id, if there is one.
///
/// # Errors
///
/// [`StoreError`] if the store fails.
pub(crate) async fn person(store: &dyn Store, id: &str) -> Result<Option<PersonView>, StoreError> {
    match store.get_record(PERSON_TYPE, person_record(id)).await? {
        Some(record) => data_of(&record).map(Some),
        None => Ok(None),
    }
}

/// Adds a person or changes one, keeping when and by whom they were added.
/// Returns them, and whether they're new.
///
/// # Errors
///
/// [`StoreError`] if the store fails.
pub(crate) async fn put_person(
    store: &dyn Store,
    slug: &str,
    request: PersonRequest,
    by: &Actor,
) -> Result<(PersonView, bool), StoreError> {
    let id = format!("person:{slug}");
    let existing = person(store, &id).await?;
    let is_new = existing.is_none();
    let view = PersonView {
        made_at: existing
            .as_ref()
            .map_or_else(Utc::now, |found| found.made_at),
        made_by: existing.map_or_else(|| by.to_string(), |found| found.made_by),
        id: id.clone(),
        name: request.name,
        role: request.role,
        disabled: request.disabled,
        day_cap_usd_micros: request.day_cap_usd_micros,
    };
    let record = Record::new(
        PERSON_TYPE.to_owned(),
        person_record(&id),
        to_data(&view)?,
        provenance(by),
    );
    store.apply(WriteOp::PutRecord(record).into()).await?;
    Ok((view, is_new))
}

/// A page of people, in record order.
///
/// # Errors
///
/// [`StoreError`] if the store fails.
pub(crate) async fn people(
    store: &dyn Store,
    page: PageRequest,
) -> Result<Page<PersonView>, StoreError> {
    let found = store
        .query_records(RecordQuery::new(PERSON_TYPE, page))
        .await?;
    let items = found
        .items
        .iter()
        .map(data_of)
        .collect::<Result<Vec<PersonView>, _>>()?;
    Ok(Page {
        items,
        has_more: found.has_more,
    })
}

/// Makes a key, kept as its digest.
///
/// # Errors
///
/// [`StoreError`] if the store fails, or there's no randomness.
pub(crate) async fn make_key(
    store: &dyn Store,
    request: KeyRequest,
    by: &Actor,
) -> Result<KeyMade, StoreError> {
    let (id, token, digest) = new_secret().map_err(StoreError::Backend)?;
    let mut scopes = request.scopes;
    scopes.sort_unstable();
    scopes.dedup();
    let key = KeyRecord {
        id: id.to_string(),
        person: request.person,
        scopes,
        name: request.name,
        digest,
        made_at: Utc::now(),
        revoked_at: None,
        expires_at: request.expires_at,
        private_by_default: request.private_by_default,
    };
    let record = Record::new(KEY_TYPE.to_owned(), id, to_data(&key)?, provenance(by));
    store.apply(WriteOp::PutRecord(record).into()).await?;
    Ok(KeyMade {
        key: key_view(key, None),
        secret: token,
    })
}

fn key_view(key: KeyRecord, last_used_at: Option<DateTime<Utc>>) -> KeyView {
    KeyView {
        id: key.id,
        person: key.person,
        scopes: key.scopes,
        name: key.name,
        made_at: key.made_at,
        last_used_at,
        revoked_at: key.revoked_at,
        expires_at: key.expires_at,
        private_by_default: key.private_by_default,
    }
}

/// A key as kept, if there is one with this id.
///
/// # Errors
///
/// [`StoreError`] if the store fails.
pub(crate) async fn key(store: &dyn Store, id: RecordId) -> Result<Option<KeyRecord>, StoreError> {
    match store.get_record(KEY_TYPE, id).await? {
        Some(record) => data_of(&record).map(Some),
        None => Ok(None),
    }
}

/// A page of keys, all of them or one person's, with when each was last
/// used.
///
/// # Errors
///
/// [`StoreError`] if the store fails.
pub(crate) async fn keys(
    store: &dyn Store,
    person: Option<&str>,
    page: PageRequest,
) -> Result<Page<KeyView>, StoreError> {
    let mut query = RecordQuery::new(KEY_TYPE, page);
    if let Some(person) = person {
        query = query.with_filter("person", FieldOp::Eq, FieldValue::Text(person.to_owned()));
    }
    let found = store.query_records(query).await?;
    let mut items = Vec::with_capacity(found.items.len());
    for record in &found.items {
        let used = store
            .get_record(KEY_USE_TYPE, use_record(record.id))
            .await?
            .map(|used| data_of::<KeyUse>(&used))
            .transpose()?;
        items.push(key_view(
            data_of(record)?,
            used.map(|used| used.last_used_at),
        ));
    }
    Ok(Page {
        items,
        has_more: found.has_more,
    })
}

/// Revokes a key. Revoking one already revoked changes nothing. Returns
/// false if there's no such key, or it isn't `person`'s when one is given.
///
/// # Errors
///
/// [`StoreError`] if the store fails.
pub(crate) async fn revoke(
    store: &dyn Store,
    id: RecordId,
    person: Option<&str>,
    by: &Actor,
) -> Result<bool, StoreError> {
    let Some(mut found) = key(store, id).await? else {
        return Ok(false);
    };
    if person.is_some_and(|person| person != found.person) {
        return Ok(false);
    }
    if found.revoked_at.is_none() {
        found.revoked_at = Some(Utc::now());
        let record = Record::new(KEY_TYPE.to_owned(), id, to_data(&found)?, provenance(by));
        store.apply(WriteOp::PutRecord(record).into()).await?;
    }
    Ok(true)
}

/// Notes that a key was used now.
///
/// # Errors
///
/// [`StoreError`] if the store fails.
pub(crate) async fn note_use(store: &dyn Store, key: RecordId) -> Result<(), StoreError> {
    let used = KeyUse {
        last_used_at: Utc::now(),
    };
    let record = Record::new(
        KEY_USE_TYPE.to_owned(),
        use_record(key),
        to_data(&used)?,
        provenance(&Actor::new(OWNER)),
    );
    store.apply(WriteOp::PutRecord(record).into()).await
}

/// The caller a key makes, or why it's refused.
///
/// # Errors
///
/// [`StoreError`] if the store fails.
pub(crate) async fn authenticate(
    store: &dyn Store,
    token: &str,
    now: DateTime<Utc>,
) -> Result<Result<Caller, Refused>, StoreError> {
    let Some((id, secret)) = parse(token) else {
        return Ok(Err(Refused::Unknown));
    };
    let Some(found) = key(store, id).await? else {
        return Ok(Err(Refused::Unknown));
    };
    let presented = digest_of(secret);
    if !bool::from(presented.as_bytes().ct_eq(found.digest.as_bytes())) {
        return Ok(Err(Refused::Unknown));
    }
    if found.revoked_at.is_some() {
        return Ok(Err(Refused::Revoked));
    }
    if found.expires_at.is_some_and(|end| end <= now) {
        return Ok(Err(Refused::Expired));
    }
    let Some(owner) = person(store, &found.person).await? else {
        return Ok(Err(Refused::Unknown));
    };
    if owner.disabled {
        return Ok(Err(Refused::Disabled));
    }
    Ok(Ok(Caller {
        actor: Actor::new(owner.id),
        role: Some(owner.role),
        scopes: found.scopes.into_iter().collect(),
        key: Some(id),
        private_by_default: found.private_by_default,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "a test")]
mod tests {
    use std::sync::Arc;

    use store::RecordReader;
    use store_memory::MemoryStore;

    use super::*;

    async fn store() -> Arc<MemoryStore> {
        let store = Arc::new(MemoryStore::new(3));
        store.register_records(&declarations()).await.unwrap();
        store
    }

    async fn someone(store: &dyn Store, role: Role) -> String {
        let request = PersonRequest {
            name: "Sam".to_owned(),
            role,
            disabled: false,
            day_cap_usd_micros: None,
        };
        let (person, is_new) = put_person(store, "sam", request, &Actor::new(OWNER))
            .await
            .unwrap();
        assert!(is_new);
        person.id
    }

    fn asking(person: &str, scopes: &[Scope]) -> KeyRequest {
        KeyRequest {
            person: person.to_owned(),
            scopes: scopes.to_vec(),
            name: "laptop".to_owned(),
            expires_at: None,
            private_by_default: false,
        }
    }

    #[test]
    fn each_role_allows_what_the_table_says() {
        use Scope::{Admin, Chat, Read, Run};
        let allowed = |role: Role| {
            [Read, Chat, Run, Admin]
                .into_iter()
                .filter(|scope| role.allows(*scope))
                .collect::<Vec<_>>()
        };
        assert_eq!(allowed(Role::Viewer), [Read]);
        assert_eq!(allowed(Role::Member), [Read, Chat, Run]);
        assert_eq!(allowed(Role::Admin), [Read, Chat, Run, Admin]);
    }

    #[tokio::test]
    async fn a_made_key_authenticates_and_its_secret_isnt_kept() {
        let store = store().await;
        let person = someone(store.as_ref(), Role::Member).await;
        let made = make_key(
            store.as_ref(),
            asking(&person, &[Scope::Read, Scope::Chat]),
            &Actor::new(OWNER),
        )
        .await
        .unwrap();
        assert!(made.secret.starts_with(KEY_PREFIX));
        let id = RecordId::from_uuid(Uuid::try_parse(&made.key.id).unwrap());
        let kept = store.get_record(KEY_TYPE, id).await.unwrap().unwrap();
        let secret = made.secret.rsplit('_').next().unwrap();
        assert!(!kept.data.to_string().contains(secret));

        let caller = authenticate(store.as_ref(), &made.secret, Utc::now())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(caller.actor.as_str(), "person:sam");
        assert!(caller.may(Scope::Chat).is_ok());
        assert!(caller.may(Scope::Run).unwrap_err().contains("run scope"));
    }

    #[tokio::test]
    async fn a_wrong_secret_and_an_unknown_id_look_the_same() {
        let store = store().await;
        let person = someone(store.as_ref(), Role::Member).await;
        let made = make_key(
            store.as_ref(),
            asking(&person, &[Scope::Read]),
            &Actor::new(OWNER),
        )
        .await
        .unwrap();
        let (head, _) = made.secret.rsplit_once('_').unwrap();
        let wrong = format!("{head}_{}", "0".repeat(64));
        let unknown = format!("{KEY_PREFIX}{}_{}", Uuid::now_v7().simple(), "0".repeat(64));
        for token in [wrong.as_str(), unknown.as_str(), "exk_nonsense"] {
            let refused = authenticate(store.as_ref(), token, Utc::now())
                .await
                .unwrap();
            assert_eq!(refused, Err(Refused::Unknown), "{token}");
        }
    }

    #[tokio::test]
    async fn revoked_expired_and_disabled_keys_stop_at_once() {
        let store = store().await;
        let person = someone(store.as_ref(), Role::Admin).await;
        let by = Actor::new(OWNER);
        let made = make_key(store.as_ref(), asking(&person, &[Scope::Read]), &by)
            .await
            .unwrap();
        let id = RecordId::from_uuid(Uuid::try_parse(&made.key.id).unwrap());
        let later = Utc::now() + chrono::Duration::days(1);

        let mut ending = asking(&person, &[Scope::Read]);
        ending.expires_at = Some(Utc::now());
        let ended = make_key(store.as_ref(), ending, &by).await.unwrap();
        let refused = authenticate(store.as_ref(), &ended.secret, later)
            .await
            .unwrap();
        assert_eq!(refused, Err(Refused::Expired));

        let disable = PersonRequest {
            name: "Sam".to_owned(),
            role: Role::Admin,
            disabled: true,
            day_cap_usd_micros: None,
        };
        put_person(store.as_ref(), "sam", disable, &by)
            .await
            .unwrap();
        let refused = authenticate(store.as_ref(), &made.secret, Utc::now())
            .await
            .unwrap();
        assert_eq!(refused, Err(Refused::Disabled));

        assert!(revoke(store.as_ref(), id, None, &by).await.unwrap());
        let refused = authenticate(store.as_ref(), &made.secret, Utc::now())
            .await
            .unwrap();
        assert_eq!(refused, Err(Refused::Revoked));
        // Someone else's key, or none at all, isn't theirs to revoke.
        assert!(
            !revoke(store.as_ref(), id, Some("person:other"), &by)
                .await
                .unwrap()
        );
        assert!(
            !revoke(store.as_ref(), RecordId::generate(), None, &by)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_lowered_role_takes_a_scope_away_without_touching_the_key() {
        let store = store().await;
        let person = someone(store.as_ref(), Role::Admin).await;
        let by = Actor::new(OWNER);
        let made = make_key(
            store.as_ref(),
            asking(&person, &[Scope::Admin, Scope::Read]),
            &by,
        )
        .await
        .unwrap();
        let lower = PersonRequest {
            name: "Sam".to_owned(),
            role: Role::Viewer,
            disabled: false,
            day_cap_usd_micros: None,
        };
        let (changed, is_new) = put_person(store.as_ref(), "sam", lower, &by).await.unwrap();
        assert!(!is_new);
        assert_eq!(changed.made_by, OWNER);
        let caller = authenticate(store.as_ref(), &made.secret, Utc::now())
            .await
            .unwrap()
            .unwrap();
        assert!(caller.may(Scope::Admin).unwrap_err().contains("viewer"));
        assert_eq!(caller.view().scopes, [Scope::Read]);
    }

    #[tokio::test]
    async fn only_a_person_pays_and_a_disabled_or_unknown_one_may_spend_nothing() {
        let store = store().await;
        let store = store.as_ref();
        for actor in [None, Some(OWNER), Some("schedule:daily"), Some("harness")] {
            assert_eq!(payer(store, 9, actor).await, None, "{actor:?}");
        }
        let person = someone(store, Role::Member).await;
        let found = payer(store, 9, Some(&person)).await.unwrap();
        assert_eq!(
            found.day_cap_usd_micros(),
            9,
            "the default, without their own"
        );
        assert_eq!(found.account(), Payer::account_of(&person));
        let unknown = payer(store, 9, Some("person:nobody")).await.unwrap();
        assert_eq!(unknown.day_cap_usd_micros(), 0);
        let disable = PersonRequest {
            name: "Sam".to_owned(),
            role: Role::Member,
            disabled: true,
            day_cap_usd_micros: Some(5),
        };
        put_person(store, "sam", disable, &Actor::new(OWNER))
            .await
            .unwrap();
        let disabled = payer(store, 9, Some(&person)).await.unwrap();
        assert_eq!(disabled.day_cap_usd_micros(), 0);
    }

    #[tokio::test]
    async fn a_use_is_noted_beside_the_key() {
        let store = store().await;
        let person = someone(store.as_ref(), Role::Member).await;
        let by = Actor::new(OWNER);
        let made = make_key(store.as_ref(), asking(&person, &[Scope::Read]), &by)
            .await
            .unwrap();
        let page = PageRequest::new(10, 0).unwrap();
        let listed = keys(store.as_ref(), Some(&person), page).await.unwrap();
        assert_eq!(listed.items[0].last_used_at, None);
        let id = RecordId::from_uuid(Uuid::try_parse(&made.key.id).unwrap());
        note_use(store.as_ref(), id).await.unwrap();
        let listed = keys(store.as_ref(), Some(&person), page).await.unwrap();
        assert!(listed.items[0].last_used_at.is_some());
        let others = keys(store.as_ref(), Some("person:other"), page)
            .await
            .unwrap();
        assert_eq!(others.items, Vec::new());
    }

    #[test]
    fn slugs_are_checked() {
        assert!(valid_slug("sam"));
        assert!(valid_slug("sam-2"));
        assert!(!valid_slug("-sam"));
        assert!(!valid_slug("Sam"));
        assert!(!valid_slug(""));
        assert!(!valid_slug(&"a".repeat(49)));
    }
}
