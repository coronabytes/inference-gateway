//! Outgoing mail settings and budget alerts: who is emailed when a project's or key's spend
//! crosses a threshold, and which alerts went out (see `alerts`).

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// The SMTP settings row; `password` is sealed.
#[derive(Debug, Clone)]
pub struct SmtpSettingsRow {
    pub enabled: bool,
    pub host: String,
    pub port: i32,
    pub security: String,
    pub username: Option<String>,
    pub password: Option<Vec<u8>>,
    pub from_address: String,
    pub from_name: Option<String>,
    pub console_url: Option<String>,
    pub updated_at: DateTime<Utc>,
}

pub async fn get_smtp_settings(pool: &PgPool) -> sqlx::Result<Option<SmtpSettingsRow>> {
    sqlx::query_as!(
        SmtpSettingsRow,
        "SELECT enabled, host, port, security, username, password, from_address, from_name, console_url, updated_at
         FROM smtp_settings WHERE id = 1"
    )
    .fetch_optional(pool)
    .await
}

pub async fn save_smtp_settings(pool: &PgPool, s: &SmtpSettingsRow) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO smtp_settings (id, enabled, host, port, security, username, password, from_address, from_name, console_url, updated_at)
         VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9, now())
         ON CONFLICT (id) DO UPDATE SET
             enabled = EXCLUDED.enabled, host = EXCLUDED.host, port = EXCLUDED.port, security = EXCLUDED.security,
             username = EXCLUDED.username, password = EXCLUDED.password, from_address = EXCLUDED.from_address,
             from_name = EXCLUDED.from_name, console_url = EXCLUDED.console_url, updated_at = now()",
        s.enabled,
        s.host,
        s.port,
        s.security,
        s.username,
        s.password,
        s.from_address,
        s.from_name,
        s.console_url,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whose budget an alert watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertOwner {
    Project(Uuid),
    Key(Uuid),
}

/// The alert rule of a project or key: thresholds in percent of the budget, and the people to email.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BudgetAlertRow {
    pub project_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub thresholds: Vec<i16>,
    pub recipients: Vec<Uuid>,
}

pub async fn get_budget_alert(pool: &PgPool, owner: AlertOwner) -> sqlx::Result<Option<BudgetAlertRow>> {
    let (project, key) = match owner {
        AlertOwner::Project(id) => (Some(id), None),
        AlertOwner::Key(id) => (None, Some(id)),
    };
    sqlx::query_as!(
        BudgetAlertRow,
        "SELECT project_id, key_id, thresholds, recipients FROM budget_alerts
         WHERE project_id IS NOT DISTINCT FROM $1 AND key_id IS NOT DISTINCT FROM $2",
        project,
        key,
    )
    .fetch_optional(pool)
    .await
}

/// Saves the rule, or removes it when it has no thresholds or no recipients.
pub async fn save_budget_alert(pool: &PgPool, owner: AlertOwner, thresholds: &[i16], recipients: &[Uuid]) -> sqlx::Result<()> {
    let (project, key) = match owner {
        AlertOwner::Project(id) => (Some(id), None),
        AlertOwner::Key(id) => (None, Some(id)),
    };
    let mut tx = pool.begin().await?;
    sqlx::query!("DELETE FROM budget_alerts WHERE project_id IS NOT DISTINCT FROM $1 AND key_id IS NOT DISTINCT FROM $2", project, key)
        .execute(&mut *tx)
        .await?;
    if !thresholds.is_empty() && !recipients.is_empty() {
        sqlx::query!(
            "INSERT INTO budget_alerts (project_id, key_id, thresholds, recipients, updated_at) VALUES ($1, $2, $3, $4, now())",
            project,
            key,
            thresholds,
            recipients,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

pub async fn list_budget_alerts(pool: &PgPool) -> sqlx::Result<Vec<BudgetAlertRow>> {
    sqlx::query_as!(BudgetAlertRow, "SELECT project_id, key_id, thresholds, recipients FROM budget_alerts").fetch_all(pool).await
}

/// Claims an alert: true when this call recorded it, false when it went out already.
pub async fn claim_budget_alert(pool: &PgPool, spender: &str, window_id: &str, threshold: i16) -> sqlx::Result<bool> {
    let r = sqlx::query!(
        "INSERT INTO budget_alerts_sent (spender, window_id, threshold, sent_at) VALUES ($1, $2, $3, now())
         ON CONFLICT DO NOTHING",
        spender,
        window_id,
        threshold,
    )
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Gives claimed alerts back, so the next check sends them again (the email failed).
pub async fn release_budget_alerts(pool: &PgPool, spender: &str, window_id: &str, thresholds: &[i16]) -> sqlx::Result<()> {
    sqlx::query!(
        "DELETE FROM budget_alerts_sent WHERE spender = $1 AND window_id = $2 AND threshold = ANY($3)",
        spender,
        window_id,
        thresholds
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Forgets the alerts sent in these budget periods (reset spend starts them over).
pub async fn forget_budget_alerts(pool: &PgPool, spender: &str, window_ids: &[String]) -> sqlx::Result<()> {
    sqlx::query!("DELETE FROM budget_alerts_sent WHERE spender = $1 AND window_id = ANY($2)", spender, window_ids).execute(pool).await?;
    Ok(())
}

/// Removes sent alerts older than any budget period (a month, with margin).
pub async fn prune_budget_alerts_sent(pool: &PgPool) -> sqlx::Result<u64> {
    let r = sqlx::query!("DELETE FROM budget_alerts_sent WHERE sent_at < now() - interval '40 days'").execute(pool).await?;
    Ok(r.rows_affected())
}

/// The email addresses of these people, by id; people without one, or removed, are left out.
pub async fn people_emails(pool: &PgPool, ids: &[Uuid]) -> sqlx::Result<Vec<(Uuid, String, Option<String>)>> {
    let rows = sqlx::query!("SELECT id, email AS \"email!\", name FROM admin_users WHERE id = ANY($1) AND email IS NOT NULL", ids)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.email, r.name)).collect())
}

pub async fn sealed_smtp_password(db: impl sqlx::PgExecutor<'_>) -> sqlx::Result<Option<Vec<u8>>> {
    let row = sqlx::query!("SELECT password FROM smtp_settings WHERE id = 1 FOR UPDATE").fetch_optional(db).await?;
    Ok(row.and_then(|r| r.password))
}

pub async fn set_smtp_password(db: impl sqlx::PgExecutor<'_>, password: &[u8]) -> sqlx::Result<()> {
    sqlx::query!("UPDATE smtp_settings SET password = $1 WHERE id = 1", password).execute(db).await?;
    Ok(())
}
