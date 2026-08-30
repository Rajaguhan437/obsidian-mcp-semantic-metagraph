//! Periodic note tool — unified handler for daily, weekly, monthly, quarterly, yearly notes.

use chrono::NaiveDate;
use rmcp::model::ErrorCode;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::models::NotePeriod;
use crate::vault::Vault;

fn parse_date(date_str: &str) -> Result<NaiveDate, rmcp::ErrorData> {
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d").map_err(|_| {
        rmcp::ErrorData::new(
            ErrorCode::INVALID_PARAMS,
            format!("Invalid date '{date_str}'; expected YYYY-MM-DD"),
            None::<serde_json::Value>,
        )
    })
}

/// Parameters for the `periodic_get` tool.
#[derive(Deserialize, JsonSchema, Default)]
pub struct PeriodicGetParams {
    /// Period type: daily, weekly, monthly, quarterly, yearly.
    pub period: NotePeriod,
    /// ISO date (YYYY-MM-DD). Defaults to today.
    #[serde(default)]
    pub date: Option<String>,
}

/// Parameters for the `periodic_list` tool.
#[derive(Deserialize, JsonSchema, Default)]
pub struct PeriodicListParams {
    /// Period type: daily, weekly, monthly, quarterly, yearly.
    pub period: NotePeriod,
    /// Maximum number of notes to return (default: 10).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for the `periodic_create` tool.
///
/// Creation is a separate tool from the two read operations because
/// `OBSIDIAN_TOOLS` filters by tool name: while all three shared one name, a
/// read-only profile had to exclude the reads in order to exclude the write.
#[derive(Deserialize, JsonSchema, Default)]
pub struct PeriodicCreateParams {
    /// Period type: daily, weekly, monthly, quarterly, yearly.
    pub period: NotePeriod,
    /// ISO date (YYYY-MM-DD). Defaults to today.
    #[serde(default)]
    pub date: Option<String>,
    /// Custom content; overrides template expansion.
    #[serde(default)]
    pub content: Option<String>,
}

/// Read the periodic note for a date.
pub async fn periodic_get(
    vault: &Vault,
    params: PeriodicGetParams,
) -> Result<String, rmcp::ErrorData> {
    let date = params.date.map(|s| parse_date(&s)).transpose()?;
    Ok(vault.get_periodic_note(&params.period, date)?)
}

/// List recent periodic notes, newest first.
pub async fn periodic_list(
    vault: &Vault,
    params: PeriodicListParams,
) -> Result<String, rmcp::ErrorData> {
    let limit = params.limit.unwrap_or(10);
    let paths = vault.list_recent_periodic_notes(&params.period, limit)?;

    let items: Vec<serde_json::Value> = paths
        .into_iter()
        .map(|p| {
            let date = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            serde_json::json!({ "path": p.to_string_lossy(), "date": date })
        })
        .collect();

    serde_json::to_string_pretty(&items).map_err(|e| {
        rmcp::ErrorData::new(
            ErrorCode::INTERNAL_ERROR,
            e.to_string(),
            None::<serde_json::Value>,
        )
    })
}

/// Create the periodic note for a date, from template or custom content.
pub async fn periodic_create(
    vault: &Vault,
    params: PeriodicCreateParams,
) -> Result<String, rmcp::ErrorData> {
    let date = params.date.map(|s| parse_date(&s)).transpose()?;
    let path = vault.create_periodic_note(&params.period, date, params.content.as_deref())?;
    Ok(format!("Created: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_helpers::{create_test_vault, test_config};

    fn setup_daily_config(dir: &std::path::Path) {
        create_test_vault(dir);
        let daily_dir = dir.join("Daily");
        fs::create_dir_all(&daily_dir).unwrap();
        fs::write(
            dir.join(".obsidian/daily-notes.json"),
            r#"{"format":"YYYY-MM-DD","folder":"Daily"}"#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn list_returns_empty_array() {
        let dir = tempfile::tempdir().unwrap();
        setup_daily_config(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();

        let result = periodic_list(
            &vault,
            PeriodicListParams {
                limit: Some(5),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let items: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn create_then_get() {
        let dir = tempfile::tempdir().unwrap();
        setup_daily_config(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();

        let msg = periodic_create(
            &vault,
            PeriodicCreateParams {
                date: Some("2026-01-15".into()),
                content: Some("hello periodic".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(msg.contains("Created"));

        let content = periodic_get(
            &vault,
            PeriodicGetParams {
                date: Some("2026-01-15".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(content.contains("hello periodic"));
    }

    /// Reading and creating are separate tools so a read-only profile can keep
    /// the reads. While all three shared one name, excluding the write meant
    /// excluding `get` and `list` too.
    #[tokio::test]
    async fn reads_do_not_create_the_note() {
        let dir = tempfile::tempdir().unwrap();
        setup_daily_config(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();

        let before = fs::read_dir(dir.path().join("Daily")).unwrap().count();

        let _ = periodic_get(
            &vault,
            PeriodicGetParams {
                date: Some("2026-03-09".into()),
                ..Default::default()
            },
        )
        .await;
        let _ = periodic_list(&vault, PeriodicListParams::default()).await;

        assert_eq!(
            before,
            fs::read_dir(dir.path().join("Daily")).unwrap().count(),
            "periodic reads must not write to the vault"
        );
    }

    #[tokio::test]
    async fn edit_rejects_an_unparseable_date() {
        let dir = tempfile::tempdir().unwrap();
        setup_daily_config(dir.path());
        let vault = Vault::open(&test_config(dir.path())).await.unwrap();

        let result = periodic_create(
            &vault,
            PeriodicCreateParams {
                date: Some("15-01-2026".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(result.unwrap_err().message.contains("Invalid date"));
    }
}
