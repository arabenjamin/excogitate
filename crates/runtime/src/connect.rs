//! MCP on stdin and stdout for a server that's already running. The store
//! takes one process, so this opens nothing of its own and relays each message
//! to the server's `/mcp`.

use std::time::Duration;

use plugin_api::read_line;
use plugin_api::rpc::{PARSE_ERROR, SERVER_ERROR, failure};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

fn error_line(id: &Value, code: i64, message: &str) -> String {
    failure(id, code, message).to_string()
}

/// What to tell the client about a reply that isn't an MCP message.
fn explain(status: StatusCode) -> String {
    match status {
        StatusCode::UNAUTHORIZED => "the server refused the owner key".to_owned(),
        StatusCode::SERVICE_UNAVAILABLE => "the server is busy; try again shortly".to_owned(),
        other => format!("the server answered {other}"),
    }
}

async fn relay_one(
    client: &reqwest::Client,
    endpoint: &str,
    key: &str,
    line: Vec<u8>,
) -> Option<String> {
    let id = match serde_json::from_slice::<Value>(&line) {
        Ok(message) => message.get("id").cloned(),
        Err(_) => return Some(error_line(&Value::Null, PARSE_ERROR, "not JSON")),
    };
    let sent = client
        .post(endpoint)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .body(line)
        .send()
        .await;
    // Anything a notification can't be told is only worth a line in the log.
    let complain = |problem: String| {
        if let Some(id) = &id {
            Some(error_line(id, SERVER_ERROR, &problem))
        } else {
            eprintln!("{problem}");
            None
        }
    };
    let Ok(response) = sent else {
        return complain(format!("can't reach the server at {endpoint}"));
    };
    let status = response.status();
    let Ok(body) = response.text().await else {
        return complain(format!("the connection to {endpoint} broke"));
    };
    let is_mcp =
        serde_json::from_str::<Value>(&body).is_ok_and(|reply| reply.get("jsonrpc").is_some());
    if is_mcp {
        Some(body)
    } else if status == StatusCode::ACCEPTED {
        None
    } else {
        complain(explain(status))
    }
}

/// Relays MCP from `reader` to the server at `url`, and its replies to
/// `writer`, until `reader` ends. A server that can't be reached is an error
/// on that message and doesn't end the session.
///
/// # Errors
///
/// The I/O error, if reading or writing fails or the client can't be built.
pub async fn relay<R, W>(url: &str, key: &str, reader: R, mut writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(std::io::Error::other)?;
    let endpoint = format!("{}/mcp", url.trim_end_matches('/'));
    let mut reader = BufReader::new(reader);
    while let Some(line) = read_line(&mut reader).await? {
        if let Some(reply) = relay_one(&client, &endpoint, key, line).await {
            writer.write_all(reply.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
    }
    Ok(())
}
