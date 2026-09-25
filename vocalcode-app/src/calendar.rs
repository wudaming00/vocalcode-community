//! Optional Google Calendar read-only desktop connector. Native OAuth/PKCE,
//! bounded upcoming-event snapshots, no calendar writes and no audio access.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

const SCOPE: &str = "https://www.googleapis.com/auth/calendar.events.readonly";
const MAX_RESPONSE: u64 = 2 * 1024 * 1024;
const MAX_EVENTS: usize = 2000;
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Connection {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    identity: String,
    calendar_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Event {
    pub key: String,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub join_url: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
struct Cache {
    updated_ms: i64,
    calendar_id: String,
    events: Vec<Event>,
}

#[derive(Default)]
pub(crate) struct ReminderSnapshot {
    pub(crate) connected: bool,
    updated_ms: i64,
    events: Vec<Event>,
}
impl ReminderSnapshot {
    pub(crate) fn candidate(
        &self,
        now_ms: i64,
    ) -> Option<crate::meeting_reminder::MeetingCandidate> {
        if !self.connected
            || self.updated_ms <= 0
            || now_ms < self.updated_ms
            || now_ms - self.updated_ms > 15 * 60 * 1000
        {
            return None;
        }
        let event = self.events.iter().find(|e| {
            e.start_ms >= now_ms - 5 * 60 * 1000
                && e.start_ms <= now_ms + 5 * 60 * 1000
                && e.end_ms > now_ms
        })?;
        Some(crate::meeting_reminder::MeetingCandidate {
            app_key: "calendar:google".into(),
            instance_key: format!("calendar:{}", event.key),
            app_name: "Google Calendar".into(),
            suggested_title: event.title.clone(),
            strength: crate::meeting_reminder::Strength::Calendar,
            foreground: false,
            conference_key: event
                .join_url
                .as_deref()
                .and_then(vocalcode_platform::meeting_presence::conference_url)
                .as_ref()
                .and_then(crate::meeting_reminder::conference_key),
        })
    }
}
#[derive(Default, Serialize, Deserialize)]
struct Link {
    event: Option<Event>,
}
fn path(base: &Path, name: &str) -> Result<PathBuf, String> {
    crate::paths::ensure_trusted_data_subdir(base, Path::new("calendar"))
        .map(|p| p.join(name))
        .map_err(|_| "Cannot access local calendar storage.".into())
}
fn read_private<T: serde::de::DeserializeOwned + Default>(
    base: &Path,
    name: &str,
) -> Result<T, String> {
    match crate::read_bounded_bytes(&path(base, name)?, MAX_RESPONSE as usize) {
        Ok(bytes) => {
            let envelope: Value =
                serde_json::from_slice(&crate::diagnostics::protect(&bytes, false)?)
                    .map_err(|_| "Cannot parse local calendar storage.")?;
            if envelope["purpose"] != format!("vocalcode-calendar-v1:{name}") {
                return Err("Calendar storage purpose mismatch.".into());
            }
            serde_json::from_value(envelope["data"].clone())
                .map_err(|_| "Unsupported local calendar storage.".into())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(_) => Err("Cannot read local calendar storage; it was left untouched.".into()),
    }
}
fn save_private<T: Serialize>(base: &Path, name: &str, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(
        &json!({"purpose":format!("vocalcode-calendar-v1:{name}"),"data":value}),
    )
    .map_err(|_| "Cannot serialize calendar data")?;
    if bytes.len() > MAX_RESPONSE as usize - 4096 {
        return Err("Local calendar snapshot is too large.".into());
    }
    crate::storage::atomic_write(
        &path(base, name)?,
        crate::diagnostics::protect(&bytes, true)?,
    )
    .map_err(|_| "Cannot save account-encrypted calendar storage.".into())
}
fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "Secure randomness unavailable")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn parse_client(bytes: &[u8]) -> Result<Connection, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| "Choose the downloaded Google desktop OAuth JSON file.")?;
    let client = &value["installed"];
    let id = client["client_id"].as_str().unwrap_or("");
    let secret = client["client_secret"].as_str().unwrap_or("");
    if id.len() > 256
        || !id.ends_with(".apps.googleusercontent.com")
        || secret.is_empty()
        || secret.len() > 512
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
        || secret.chars().any(char::is_control)
    {
        return Err("This is not a valid installed/desktop OAuth client file.".into());
    }
    // Ignore embedded endpoints: credentials cannot redirect requests off Google.
    Ok(Connection {
        client_id: id.into(),
        client_secret: secret.into(),
        calendar_id: "primary".into(),
        ..Default::default()
    })
}
fn auth_url(connection: &Connection, redirect: &str, state: &str, verifier: &str) -> String {
    let mut url =
        url::Url::parse("https://accounts.google.com/o/oauth2/v2/auth").expect("fixed endpoint");
    url.query_pairs_mut().extend_pairs([
        ("client_id", connection.client_id.as_str()),
        ("redirect_uri", redirect),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("state", state),
        (
            "code_challenge",
            URL_SAFE_NO_PAD
                .encode(Sha256::digest(verifier.as_bytes()))
                .as_str(),
        ),
        ("code_challenge_method", "S256"),
    ]);
    url.into()
}
fn callback(line: &str, state: &str) -> Result<Option<String>, String> {
    let mut parts = line.split_whitespace();
    if parts.next() != Some("GET") {
        return Ok(None);
    }
    let Some(target) = parts.next() else {
        return Ok(None);
    };
    if !target.starts_with("/?") || target.len() > 8192 {
        return Ok(None);
    }
    let url = url::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| "Invalid OAuth callback")?;
    let mut values = std::collections::HashMap::new();
    for (key, value) in url.query_pairs() {
        if values.insert(key.to_string(), value.to_string()).is_some() {
            return Ok(None);
        }
    }
    if values.get("state").map(String::as_str) != Some(state) {
        return Ok(None);
    }
    if values.contains_key("error") {
        return Err("Google sign-in was cancelled or denied. No calendar connected.".into());
    }
    Ok(values
        .remove("code")
        .filter(|c| !c.is_empty() && c.len() < 4096))
}
fn read_request_line(stream: &mut impl Read) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 512];
    while bytes.len() < 8192 && Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(count) => {
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.iter().position(|&b| b == b'\n') {
                    return std::str::from_utf8(&bytes[..end]).ok().map(str::to_owned);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return None,
        }
    }
    None
}
fn token_request(pairs: &[(&str, &str)]) -> Result<Value, String> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().copied())
        .finish();
    let mut response = ureq::post("https://oauth2.googleapis.com/token")
        .config()
        .https_only(true)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body)
        .map_err(|_| {
            "Google authorization failed. Check the OAuth client, connection or sign in again."
        })?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_vec()
        .map_err(|_| "Invalid Google authorization response")?;
    serde_json::from_slice(&bytes).map_err(|_| "Invalid Google authorization response".into())
}
fn connect(base: &Path, status: &crate::webui::RuntimeStatus) -> Result<(), String> {
    let mut connection: Connection = read_private(base, "connection.vcs")?;
    if connection.client_id.is_empty() {
        return Err("First choose a Google desktop OAuth client JSON file.".into());
    }
    let verifier = random_token()?;
    let state = random_token()?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|_| "Cannot open the local sign-in callback")?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "Cannot configure sign-in callback")?;
    let redirect = format!(
        "http://127.0.0.1:{}/",
        listener
            .local_addr()
            .map_err(|_| "Cannot read sign-in port")?
            .port()
    );
    if !crate::webui::open_url(&auth_url(&connection, &redirect, &state, &verifier)) {
        return Err("Cannot open the system browser for Google sign-in.".into());
    }
    let started = Instant::now();
    let code = loop {
        if status.shutdown.load(Ordering::Acquire) || status.calendar_cancel.load(Ordering::Acquire)
        {
            return Err("Calendar sign-in cancelled.".into());
        }
        if started.elapsed() > Duration::from_secs(180) {
            return Err("Google sign-in timed out. Try again when ready.".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                let Some(line) = read_request_line(&mut stream) else {
                    continue;
                };
                let reply = callback(&line, &state);
                let valid = !matches!(reply, Ok(None));
                let body = if valid {
                    "Return to VocalCode to check the connection."
                } else {
                    "Invalid sign-in callback."
                };
                let _=write!(stream,"HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",if valid{"200 OK"}else{"400 Bad Request"},body.len(),body);
                if let Some(code) = reply? {
                    break code;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(_) => return Err("Local sign-in callback stopped.".into()),
        }
    };
    let result = token_request(&[
        ("client_id", &connection.client_id),
        ("client_secret", &connection.client_secret),
        ("code", &code),
        ("code_verifier", &verifier),
        ("redirect_uri", &redirect),
        ("grant_type", "authorization_code"),
    ])?;
    if status.calendar_cancel.load(Ordering::Acquire) || status.shutdown.load(Ordering::Acquire) {
        return Err("Calendar sign-in cancelled; tokens were not saved.".into());
    }
    if !result["scope"]
        .as_str()
        .unwrap_or("")
        .split_whitespace()
        .any(|s| s == SCOPE)
    {
        return Err("Google did not grant the requested read-only event permission.".into());
    }
    connection.refresh_token = result["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() < 8192)
        .ok_or("Google did not provide offline authorization; reconnect with consent.")?
        .into();
    connection.identity = random_token()?;
    save_private(base, "connection.vcs", &connection)?;
    save_private(base, "events.vcs", &Cache::default())
}
fn timestamp(value: &Value) -> Option<i64> {
    value["dateTime"]
        .as_str()?
        .parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_millisecond())
}
fn safe_join(value: &str) -> Option<String> {
    if value.len() > 2048 {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    if host == "meet.google.com"
        || host == "teams.microsoft.com"
        || host == "teams.live.com"
        || host == "zoom.us"
        || host.ends_with(".zoom.us")
    {
        Some(url.into())
    } else {
        None
    }
}
fn events(
    value: &Value,
    identity: &str,
    calendar: &str,
    from: i64,
    to: i64,
) -> Result<Vec<Event>, String> {
    let items = value["items"]
        .as_array()
        .ok_or("Invalid Google events response")?;
    if items.len() > MAX_EVENTS {
        return Err("Too many events in one page".into());
    }
    let mut output = Vec::new();
    for item in items {
        if item["status"] == "cancelled"
            || item["attendees"].as_array().is_some_and(|a| {
                a.iter()
                    .any(|a| a["self"] == true && a["responseStatus"] == "declined")
            })
        {
            continue;
        }
        let (Some(start), Some(end)) = (timestamp(&item["start"]), timestamp(&item["end"])) else {
            continue;
        };
        if end <= start || end <= from || start >= to {
            continue;
        }
        let Some(id) = item["id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() < 1024)
        else {
            continue;
        };
        let mut title = item["summary"]
            .as_str()
            .unwrap_or("Untitled meeting")
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>();
        while title.len() > 512 {
            title.pop();
        }
        if title.trim().is_empty() {
            title = "Untitled meeting".into();
        }
        let original = item["originalStartTime"].to_string();
        let key = format!(
            "{:x}",
            Sha256::digest(format!("{identity}\0{calendar}\0{id}\0{original}"))
        );
        let join_url = item["hangoutLink"]
            .as_str()
            .and_then(safe_join)
            .or_else(|| {
                item["conferenceData"]["entryPoints"]
                    .as_array()?
                    .iter()
                    .filter(|p| p["entryPointType"] == "video")
                    .find_map(|p| safe_join(p["uri"].as_str()?))
            });
        output.push(Event {
            key,
            title,
            start_ms: start,
            end_ms: end,
            join_url,
        });
    }
    Ok(output)
}
fn sync(base: &Path, status: &crate::webui::RuntimeStatus, calendar: &str) -> Result<(), String> {
    let result = sync_inner(base, status, calendar);
    if result.is_err() {
        status
            .calendar_snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .updated_ms = 0;
    }
    result
}
fn sync_inner(
    base: &Path,
    status: &crate::webui::RuntimeStatus,
    calendar: &str,
) -> Result<(), String> {
    if calendar.is_empty() || calendar.len() > 256 || calendar.chars().any(char::is_control) {
        return Err("Enter a calendar ID, or primary.".into());
    }
    let mut connection: Connection = read_private(base, "connection.vcs")?;
    if connection.refresh_token.is_empty() {
        return Err("Google Calendar is not connected.".into());
    }
    let token = token_request(&[
        ("client_id", &connection.client_id),
        ("client_secret", &connection.client_secret),
        ("refresh_token", &connection.refresh_token),
        ("grant_type", "refresh_token"),
    ])?;
    let access = token["access_token"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() < 8192)
        .ok_or("Google did not return an access token")?;
    let now = jiff::Timestamp::now();
    let from = now.as_second() - 3600;
    let to = now.as_second() + 7 * 86400;
    let mut collected = Vec::new();
    let mut page = String::new();
    let mut seen = HashSet::new();
    let mut keys = HashSet::new();
    loop {
        if status.shutdown.load(Ordering::Acquire) || status.calendar_cancel.load(Ordering::Acquire)
        {
            return Err("Calendar refresh cancelled; earlier snapshot preserved.".into());
        }
        let mut url = url::Url::parse("https://www.googleapis.com/calendar/v3/calendars/")
            .expect("fixed endpoint");
        url.path_segments_mut()
            .expect("base URL")
            .pop_if_empty()
            .push(calendar)
            .push("events");
        url.query_pairs_mut().extend_pairs([
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("maxResults", "250"),
            (
                "timeMin",
                jiff::Timestamp::from_second(from)
                    .map_err(|_| "Invalid clock")?
                    .to_string()
                    .as_str(),
            ),
            (
                "timeMax",
                jiff::Timestamp::from_second(to)
                    .map_err(|_| "Invalid clock")?
                    .to_string()
                    .as_str(),
            ),
        ]);
        if !page.is_empty() {
            url.query_pairs_mut().append_pair("pageToken", &page);
        }
        let mut response=ureq::get(url.as_str()).config().https_only(true).max_redirects(0).timeout_global(Some(Duration::from_secs(20))).build()
            .header("Authorization",&format!("Bearer {access}")).call().map_err(|_|"Could not read Google Calendar. Check connection, calendar ID or sign in again; previous snapshot is stale.")?;
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE)
            .read_to_vec()
            .map_err(|_| "Calendar response exceeded its safety limit")?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid calendar response")?;
        for event in events(
            &value,
            &connection.identity,
            calendar,
            from * 1000,
            to * 1000,
        )? {
            if keys.insert(event.key.clone()) {
                collected.push(event);
            }
        }
        if collected.len() > MAX_EVENTS {
            return Err("More than 2000 upcoming events; choose a narrower calendar. Previous snapshot preserved.".into());
        }
        let next = value["nextPageToken"].as_str().unwrap_or("");
        if next.is_empty() {
            break;
        }
        if next.len() > 4096 || seen.len() >= 20 || !seen.insert(next.to_string()) {
            return Err(
                "Invalid or excessive calendar pagination; previous snapshot preserved.".into(),
            );
        }
        page = next.into();
    }
    collected.sort_by_key(|e| e.start_ms);
    if status.shutdown.load(Ordering::Acquire) || status.calendar_cancel.load(Ordering::Acquire) {
        return Err("Calendar refresh cancelled; previous snapshot preserved.".into());
    }
    save_private(
        base,
        "events.vcs",
        &Cache {
            updated_ms: now.as_millisecond(),
            calendar_id: calendar.into(),
            events: collected,
        },
    )?;
    connection.calendar_id = calendar.into();
    save_private(base, "connection.vcs", &connection)
}
fn snapshot(base: &Path) -> Result<Value, String> {
    let connection: Connection = read_private(base, "connection.vcs")?;
    let cache: Cache = read_private(base, "events.vcs")?;
    Ok(
        json!({"configured":!connection.client_id.is_empty(),"connected":!connection.refresh_token.is_empty(),
        "calendar_id":connection.calendar_id,"snapshot_calendar_id":cache.calendar_id,"updated_ms":cache.updated_ms,"events":cache.events}),
    )
}
pub(crate) fn handle(
    base: &Path,
    status: &crate::webui::RuntimeStatus,
    v: &Value,
    input: Option<PathBuf>,
) -> Result<Value, String> {
    match v["op"].as_str().unwrap_or("") {
        "load" => {}
        "configure" => {
            let source = input.ok_or("Choose a desktop OAuth client JSON file")?;
            let bytes = crate::read_bounded_bytes(&source, 16 * 1024)
                .map_err(|_| "Cannot read client configuration")?;
            let existing: Connection = read_private(base, "connection.vcs")?;
            if !existing.refresh_token.is_empty() {
                return Err("Disconnect locally before changing the OAuth client.".into());
            }
            save_private(base, "connection.vcs", &parse_client(&bytes)?)?;
        }
        "connect" => connect(base, status)?,
        "sync" => sync(base, status, v["calendar_id"].as_str().unwrap_or("primary"))?,
        "disconnect" => {
            let mut connection: Connection = read_private(base, "connection.vcs")?;
            connection.refresh_token.clear();
            connection.identity.clear();
            save_private(base, "connection.vcs", &connection)?;
            *status
                .calendar_snapshot
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = ReminderSnapshot::default();
            save_private(base, "events.vcs", &Cache::default())?;
        }
        "open" => {
            let cache: Cache = read_private(base, "events.vcs")?;
            let event = cache
                .events
                .iter()
                .find(|e| v["key"] == e.key)
                .ok_or("Event changed. Refresh the calendar.")?;
            let url = event
                .join_url
                .as_deref()
                .and_then(safe_join)
                .ok_or("No supported HTTPS meeting link")?;
            if !crate::webui::open_url(&url) {
                return Err("Could not open the meeting link".into());
            }
        }
        "link" | "unlink" | "linked" => {
            let id = vocalcode_meeting::MeetingId::parse(v["meeting_id"].as_str().unwrap_or(""))
                .map_err(|_| "Choose a saved local meeting first")?;
            let root = crate::paths::ensure_trusted_data_subdir(base, Path::new("meetings"))
                .map_err(|_| "Cannot access local meetings")?;
            let store = vocalcode_meeting::MeetingStore::open(root)
                .map_err(|_| "Cannot open local meetings")?;
            let name = format!("meeting-{id}.vcs");
            let _guard = crate::lock_rules_writes(&path(base, &name)?)?;
            store
                .load(&id)
                .map_err(|_| "The local meeting no longer exists or cannot be read")?;
            let mut link: Link = read_private(base, &name)?;
            let revision = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&link).map_err(|_| "Invalid association")?)
            );
            if v["op"] != "linked" {
                if v["revision"].as_str() != Some(&revision) {
                    return Err(
                        "Meeting association changed. Load it again before replacing.".into(),
                    );
                }
                if status.meetings.is_active() {
                    return Err("Stop the recording before changing its saved association.".into());
                }
                let cache: Cache = read_private(base, "events.vcs")?;
                link.event = if v["op"] == "unlink" {
                    None
                } else {
                    Some(
                        cache
                            .events
                            .into_iter()
                            .find(|e| v["key"] == e.key)
                            .ok_or("Event changed. Refresh calendar first.")?,
                    )
                };
                save_private(base, &name, &link)?;
            }
            let revision = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&link).map_err(|_| "Invalid association")?)
            );
            return Ok(
                json!({"meeting_id":id.to_string(),"linked_event":link.event,"revision":revision}),
            );
        }
        _ => return Err("Unknown calendar operation".into()),
    }
    let state = snapshot(base)?;
    let cache: Cache = read_private(base, "events.vcs")?;
    *status
        .calendar_snapshot
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = ReminderSnapshot {
        connected: state["connected"] == true,
        updated_ms: cache.updated_ms,
        events: cache.events,
    };
    Ok(state)
}

pub(crate) fn refresh_saved_calendar(
    base: &Path,
    status: &crate::webui::RuntimeStatus,
) -> Result<(), String> {
    let connection: Connection = read_private(base, "connection.vcs")?;
    if connection.refresh_token.is_empty() {
        return Ok(());
    }
    sync(base, status, &connection.calendar_id)?;
    let cache: Cache = read_private(base, "events.vcs")?;
    *status
        .calendar_snapshot
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = ReminderSnapshot {
        connected: true,
        updated_ms: cache.updated_ms,
        events: cache.events,
    };
    Ok(())
}

pub(crate) fn delete_meeting_and_link(
    store: &vocalcode_meeting::MeetingStore,
    id: &vocalcode_meeting::MeetingId,
) -> Result<Option<String>, String> {
    let base = store
        .root()
        .parent()
        .ok_or("Cannot resolve meeting storage")?;
    let association = path(base, &format!("meeting-{id}.vcs"))?;
    let _guard = crate::lock_rules_writes(&association)?;
    store.delete(id).map_err(|e| e.to_string())?;
    match std::fs::remove_file(association){
        Ok(())=>Ok(None),
        Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(None),
        Err(_)=>Ok(Some("Meeting deleted, but its encrypted calendar association could not be removed. Check application-data permissions.".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_callback_lines_are_read_without_unbounded_headers() {
        struct Fragmented(std::io::Cursor<Vec<u8>>);
        impl Read for Fragmented {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let count = buf.len().min(3);
                self.0.read(&mut buf[..count])
            }
        }
        let mut input = Fragmented(std::io::Cursor::new(
            b"GET /?state=expected&code=abc HTTP/1.1\r\nHost: localhost\r\n".to_vec(),
        ));
        let line = read_request_line(&mut input).unwrap();
        assert_eq!(callback(&line, "expected").unwrap(), Some("abc".into()));
        assert!(read_request_line(&mut std::io::Cursor::new(vec![b'a'; 9000])).is_none());
    }
    #[test]
    fn deleting_meeting_removes_only_its_association() {
        let base = crate::test_support::TempDir::new("calendar-delete-test");
        let store = vocalcode_meeting::MeetingStore::open(base.join("meetings")).unwrap();
        let meeting = store
            .create(vocalcode_meeting::NewMeeting {
                title: "Test".into(),
                now_ms: 1_787_796_747_000,
                source: vocalcode_meeting::MeetingSource::Imported {
                    file_name: "test.wav".into(),
                },
                language: "en".into(),
                audio_retention: vocalcode_meeting::AudioRetention::DeleteAfterTranscription,
            })
            .unwrap();
        let association = path(&base, &format!("meeting-{}.vcs", meeting.id)).unwrap();
        crate::storage::atomic_write_new(&association, b"test fixture").unwrap();
        let unrelated = path(&base, "unrelated.vcs").unwrap();
        crate::storage::atomic_write_new(&unrelated, b"unrelated fixture").unwrap();
        assert!(delete_meeting_and_link(&store, &meeting.id)
            .unwrap()
            .is_none());
        assert!(!association.exists());
        assert!(unrelated.exists());
        assert!(store.load(&meeting.id).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn persisted_authorization_is_encrypted_and_absent_from_ui_snapshot() {
        let base = crate::test_support::TempDir::new("calendar-test");
        let connection = Connection {
            client_id: "test.apps.googleusercontent.com".into(),
            client_secret: "private-client-secret".into(),
            refresh_token: "private-refresh-token".into(),
            identity: "test".into(),
            calendar_id: "primary".into(),
        };
        save_private(&base, "connection.vcs", &connection).unwrap();
        let state = snapshot(&base).unwrap();
        assert_eq!(state["connected"], true);
        assert!(!state.to_string().contains("private"));
        let bytes = std::fs::read(path(&base, "connection.vcs").unwrap()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("private-refresh"));
        assert_eq!(
            read_private::<Connection>(&base, "connection.vcs")
                .unwrap()
                .refresh_token,
            "private-refresh-token"
        );
    }
    #[test]
    fn oauth_uses_pkce_readonly_scope_and_exact_state() {
        let c = Connection {
            client_id: "test.apps.googleusercontent.com".into(),
            ..Default::default()
        };
        let url = auth_url(&c, "http://127.0.0.1:1234/", "state", "verifier");
        assert!(url.contains("code_challenge_method=S256"));
        assert!(!url.contains("client_secret"));
        assert_eq!(
            callback("GET /?code=secret&state=wrong HTTP/1.1", "state").unwrap(),
            None
        );
        assert_eq!(
            callback("GET /?code=abc&state=state HTTP/1.1", "state").unwrap(),
            Some("abc".into())
        );
        assert!(callback("GET /?error=access_denied&state=state HTTP/1.1", "state").is_err());
        assert_eq!(
            callback("GET /?state=state&state=state&code=x HTTP/1.1", "state").unwrap(),
            None
        );
    }

    #[test]
    fn scheduled_reminders_require_fresh_cache_and_do_not_claim_attendance() {
        let mut snapshot = ReminderSnapshot {
            connected: true,
            updated_ms: 1_000_000,
            events: vec![Event {
                key: "one".into(),
                title: "Planning".into(),
                start_ms: 1_200_000,
                end_ms: 1_800_000,
                join_url: None,
            }],
        };
        assert!(snapshot.candidate(1_000_000).is_some());
        assert!(snapshot.candidate(999_999).is_none());
        assert!(snapshot.candidate(2_000_000).is_none());
        snapshot.events[0].start_ms = 3_000_000;
        assert!(snapshot.candidate(1_000_000).is_none());
    }
    #[test]
    fn event_instances_offsets_cancelled_and_declined_are_handled() {
        let base = json!({"id":"one","summary":"Planning","start":{"dateTime":"2026-09-07T09:00:00-07:00"},"end":{"dateTime":"2026-09-07T10:00:00-07:00"},"hangoutLink":"https://meet.google.com/abc-defg-hij"});
        let mut two = base.clone();
        two["id"] = json!("two");
        let mut cancelled = base.clone();
        cancelled["status"] = json!("cancelled");
        let mut declined = base.clone();
        declined["attendees"] = json!([{"self":true,"responseStatus":"declined"}]);
        let out = events(
            &json!({"items":[base.clone(),two,cancelled,declined,{"start":{"date":"2026-09-07"}}]}),
            "account",
            "primary",
            0,
            i64::MAX,
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        assert_ne!(out[0].key, out[1].key);
        assert_eq!(
            out[0].start_ms,
            "2026-09-07T16:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_millisecond()
        );
        assert!(safe_join("https://meet.google.com.evil.example/a").is_none());
        assert!(safe_join("file:///private").is_none());
        assert!(safe_join("https://evil@meet.google.com/a").is_none());
        assert_ne!(
            out[0].key,
            events(
                &json!({"items":[base]}),
                "different-account",
                "primary",
                0,
                i64::MAX
            )
            .unwrap()[0]
                .key
        );
    }
}
