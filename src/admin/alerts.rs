//! Outgoing mail settings and the budget alerts of projects and keys (see `crate::alerts`).
//! Admins only.

use axum::{
    Extension, Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::R;
use crate::{
    alerts::{self, MAX_RECIPIENTS, MAX_THRESHOLDS, THRESHOLDS},
    audit::{self, Actor},
    budget::{BudgetPeriod, BudgetStatus},
    db::{self, AlertOwner, SmtpSettingsRow},
    error::AdminError,
    mail::{Mailer, SmtpSecurity, valid_address},
    state::AppState,
};

/// The outgoing mail settings. The password is never returned.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct SmtpSettingsView {
    /// Whether the settings were ever saved.
    configured: bool,
    enabled: bool,
    host: String,
    port: u16,
    security: SmtpSecurity,
    #[schema(required)]
    username: Option<String>,
    password_set: bool,
    from_address: String,
    #[schema(required)]
    from_name: Option<String>,
    /// The console's address, for links in emails.
    #[schema(required)]
    console_url: Option<String>,
    #[schema(required)]
    updated_at: Option<DateTime<Utc>>,
}

fn smtp_view(row: Option<SmtpSettingsRow>) -> SmtpSettingsView {
    match row {
        Some(r) => SmtpSettingsView {
            configured: true,
            enabled: r.enabled,
            host: r.host,
            port: u16::try_from(r.port).unwrap_or(587),
            security: SmtpSecurity::parse(&r.security).unwrap_or(SmtpSecurity::Starttls),
            username: r.username,
            password_set: r.password.is_some(),
            from_address: r.from_address,
            from_name: r.from_name,
            console_url: r.console_url,
            updated_at: Some(r.updated_at),
        },
        None => SmtpSettingsView {
            configured: false,
            enabled: false,
            host: String::new(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: None,
            password_set: false,
            from_address: String::new(),
            from_name: Some("Inference Gateway".into()),
            console_url: None,
            updated_at: None,
        },
    }
}

/// Get email settings
///
/// The mail server budget alerts are sent through. The password is never returned.
#[utoipa::path(get, path = "/settings/smtp", tag = "Settings",
    responses((status = 200, description = "The settings.", body = SmtpSettingsView)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn get_smtp_settings(State(state): State<AppState>) -> R<Json<SmtpSettingsView>> {
    Ok(Json(smtp_view(db::get_smtp_settings(&state.db).await?)))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SmtpSettingsInput {
    enabled: bool,
    host: String,
    port: u16,
    security: SmtpSecurity,
    /// Empty or null signs in to no account.
    #[serde(default)]
    username: Option<String>,
    /// Omitted or null keeps the stored password; an empty string removes it.
    #[serde(default)]
    password: Option<String>,
    from_address: String,
    #[serde(default)]
    from_name: Option<String>,
    /// https (or http for localhost) address of the console, for links in emails.
    #[serde(default)]
    console_url: Option<String>,
}

fn trimmed(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Save email settings
#[utoipa::path(put, path = "/settings/smtp", tag = "Settings", request_body = SmtpSettingsInput,
    responses(
        (status = 200, description = "The saved settings.", body = SmtpSettingsView),
        (status = 400, description = "Invalid settings.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn put_smtp_settings(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(i): Json<SmtpSettingsInput>,
) -> R<Json<SmtpSettingsView>> {
    let existing = db::get_smtp_settings(&state.db).await?;
    let host = i.host.trim().to_string();
    // The stored password is sent to the mail server: a server on another host needs it entered
    // again, so changing the host cannot collect it.
    let moved = existing.as_ref().is_some_and(|e| e.password.is_some() && !e.host.eq_ignore_ascii_case(&host));
    let password = match i.password {
        None if moved => return Err(AdminError::bad_request("the mail server is another host now: enter the password again")),
        None => existing.as_ref().and_then(|e| e.password.clone()),
        Some(p) if p.is_empty() => None,
        Some(p) => Some(state.cfg.secret_box.seal(p.as_bytes())),
    };
    let row = SmtpSettingsRow {
        enabled: i.enabled,
        host,
        port: i.port.into(),
        security: i.security.as_str().to_string(),
        username: trimmed(i.username),
        password,
        from_address: i.from_address.trim().to_string(),
        from_name: trimmed(i.from_name),
        console_url: trimmed(i.console_url).map(|u| u.trim_end_matches('/').to_string()),
        updated_at: Utc::now(),
    };
    if row.host.is_empty() || row.host.len() > 253 || row.host.contains(|c: char| c.is_whitespace() || c == '/' || c == ':') {
        return Err(AdminError::bad_request("host must be the mail server's name or address, without a port"));
    }
    if row.port == 0 {
        return Err(AdminError::bad_request("port must be between 1 and 65535"));
    }
    if !valid_address(&row.from_address) {
        return Err(AdminError::bad_request("from_address must be an email address"));
    }
    if row.from_name.as_ref().is_some_and(|n| n.len() > 100) || row.username.as_ref().is_some_and(|n| n.len() > 320) {
        return Err(AdminError::bad_request("from_name can be at most 100 and username 320 characters"));
    }
    if let Some(url) = &row.console_url {
        let ok = reqwest::Url::parse(url).is_ok_and(|u| match u.scheme() {
            "https" => u.host_str().is_some(),
            "http" => u.host_str().is_some(),
            _ => false,
        });
        if !ok {
            return Err(AdminError::bad_request("console_url must be an http or https URL"));
        }
    }
    // Checks the security, port and password can make a transport, before anything is saved.
    Mailer::new(&state, &row).map_err(AdminError::bad_request)?;
    let before = smtp_view(existing);
    db::save_smtp_settings(&state.db, &row).await?;
    tracing::info!(enabled = row.enabled, host = %row.host, "email settings saved");
    let after = smtp_view(Some(row));
    let changes = audit::changes(&before, &after);
    if !changes.is_empty() {
        audit::Event::new("smtp_settings", "update").named("Email").details(changes.into()).record(&state, &actor).await;
    }
    Ok(Json(after))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SmtpTestInput {
    /// Where the test email goes.
    to: String,
}

/// Whether the test email went out, or the mail server's error.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct SmtpTest {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    error: Option<String>,
}

/// Send a test email
///
/// Sends a sample budget alert with the saved settings, enabled or not.
#[utoipa::path(post, path = "/settings/smtp/test", tag = "Settings", request_body = SmtpTestInput,
    responses((status = 200, description = "Whether the email was sent.", body = SmtpTest)),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn test_smtp_settings(State(state): State<AppState>, Json(i): Json<SmtpTestInput>) -> R<Json<SmtpTest>> {
    if !valid_address(&i.to) {
        return Err(AdminError::bad_request("to must be an email address"));
    }
    let Some(row) = db::get_smtp_settings(&state.db).await? else {
        return Err(AdminError::bad_request("save the settings first"));
    };
    let mailer = Mailer::new(&state, &row).map_err(AdminError::bad_request)?;
    let now = Utc::now();
    let status = BudgetStatus {
        period: BudgetPeriod::Month,
        limit_usd: 500.0,
        spent_usd: 412.37,
        resets_at: BudgetPeriod::Month.window(now).end,
        unknown: false,
    };
    let owner = alerts::Owner::Project { name: "Example project".into() };
    let mut mail = alerts::budget_email(&alerts::BudgetEmail {
        owner: &owner,
        threshold: 80,
        status: &status,
        console_url: mailer.console_url.as_deref(),
    });
    mail.subject = format!("[Test] {}", mail.subject);
    Ok(Json(match mailer.send(&[(i.to.trim().to_string(), None)], &mail).await {
        Ok(()) => SmtpTest { ok: true, error: None },
        Err(e) => SmtpTest { ok: false, error: Some(e) },
    }))
}

/// A project's or key's budget alerts.
#[derive(Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetAlerts {
    /// Percent of the budget (1 to 100) at which an email goes out, once per budget period each.
    thresholds: Vec<i16>,
    /// People (ids from `GET /admins`) who get the emails, at the email address they signed in with.
    recipients: Vec<Uuid>,
}

async fn alerts_of(state: &AppState, owner: AlertOwner) -> R<BudgetAlerts> {
    let row = db::get_budget_alert(&state.db, owner).await?.unwrap_or_default();
    Ok(BudgetAlerts { thresholds: row.thresholds, recipients: row.recipients })
}

/// Validates and saves `i`, records the change, and returns what is stored.
async fn save_alerts(
    state: &AppState,
    actor: &Actor,
    owner: AlertOwner,
    target: (&'static str, Uuid, String),
    i: BudgetAlerts,
) -> R<BudgetAlerts> {
    let mut thresholds = i.thresholds;
    thresholds.sort_unstable();
    thresholds.dedup();
    if thresholds.iter().any(|t| !THRESHOLDS.contains(t)) {
        return Err(AdminError::bad_request("thresholds must be percents from 1 to 100"));
    }
    let mut recipients = i.recipients;
    recipients.sort_unstable();
    recipients.dedup();
    if thresholds.len() > MAX_THRESHOLDS || recipients.len() > MAX_RECIPIENTS {
        return Err(AdminError::bad_request(format!("at most {MAX_THRESHOLDS} thresholds and {MAX_RECIPIENTS} recipients")));
    }
    if !recipients.is_empty() {
        let known = db::people_emails(&state.db, &recipients).await?;
        if known.len() != recipients.len() {
            return Err(AdminError::bad_request("every recipient must be a person who signed in with an email address"));
        }
    }
    let before = alerts_of(state, owner).await?;
    db::save_budget_alert(&state.db, owner, &thresholds, &recipients).await?;
    let after = alerts_of(state, owner).await?;
    let changes = audit::changes(&before, &after);
    if !changes.is_empty() {
        let (kind, id, name) = target;
        audit::Event::new(kind, "update_alerts").on(id, name).details(changes.into()).record(state, actor).await;
    }
    Ok(after)
}

/// Get project budget alerts
#[utoipa::path(get, path = "/projects/{id}/budget-alerts", tag = "Virtual keys", params(("id" = Uuid, Path)),
    responses(
        (status = 200, description = "The alerts; empty lists when there are none.", body = BudgetAlerts),
        (status = 404, description = "No such project.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn get_project_alerts(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<BudgetAlerts>> {
    db::get_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    Ok(Json(alerts_of(&state, AlertOwner::Project(id)).await?))
}

/// Save project budget alerts
///
/// Replaces the project's alerts. No thresholds or no recipients removes them.
#[utoipa::path(put, path = "/projects/{id}/budget-alerts", tag = "Virtual keys", params(("id" = Uuid, Path)),
    request_body = BudgetAlerts,
    responses(
        (status = 200, description = "The saved alerts.", body = BudgetAlerts),
        (status = 400, description = "Invalid alerts.", body = crate::error::AdminErrorBody),
        (status = 404, description = "No such project.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn put_project_alerts(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(i): Json<BudgetAlerts>,
) -> R<Json<BudgetAlerts>> {
    let p = db::get_project(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    Ok(Json(save_alerts(&state, &actor, AlertOwner::Project(id), ("project", id, p.name), i).await?))
}

/// Get key budget alerts
#[utoipa::path(get, path = "/keys/{id}/budget-alerts", tag = "Virtual keys", params(("id" = Uuid, Path)),
    responses(
        (status = 200, description = "The alerts; empty lists when there are none.", body = BudgetAlerts),
        (status = 404, description = "No such key.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn get_key_alerts(State(state): State<AppState>, Path(id): Path<Uuid>) -> R<Json<BudgetAlerts>> {
    db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    Ok(Json(alerts_of(&state, AlertOwner::Key(id)).await?))
}

/// Save key budget alerts
///
/// Replaces the key's alerts. No thresholds or no recipients removes them.
#[utoipa::path(put, path = "/keys/{id}/budget-alerts", tag = "Virtual keys", params(("id" = Uuid, Path)),
    request_body = BudgetAlerts,
    responses(
        (status = 200, description = "The saved alerts.", body = BudgetAlerts),
        (status = 400, description = "Invalid alerts.", body = crate::error::AdminErrorBody),
        (status = 404, description = "No such key.", body = crate::error::AdminErrorBody),
    ),
    security(("master_key" = []), ("admin_key" = []), ("admin_session" = [])))]
pub(super) async fn put_key_alerts(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<Uuid>,
    Json(i): Json<BudgetAlerts>,
) -> R<Json<BudgetAlerts>> {
    let k = db::get_key(&state.db, id).await?.ok_or_else(AdminError::not_found)?;
    Ok(Json(save_alerts(&state, &actor, AlertOwner::Key(id), ("key", id, k.name), i).await?))
}
