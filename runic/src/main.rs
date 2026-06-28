use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{Datelike, Duration, NaiveDate};
use runic_agent::RunContext;
use runic_foundry::{Assembly, assemble};
use runic_provider::AnthropicDriver;
use runic_tool::{Tool, ToolContext, ToolResult};
use serde_json::json;

const DEFAULT_VISIT_DATE: &str = "2026-04-29";
const DEFAULT_REPORTLOG_SKILLS_DIR: &str =
    "/Users/machache/Developper/back-agents/agents/reportlog/skills";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let transcript = std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    let transcript = if transcript.is_empty() {
        "Visite chez la ferme demo. L'agriculteur veut un rappel avant vendredi pour trancher sur le desherbage.".to_string()
    } else {
        transcript
    };

    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("CLAUDE_API_KEY"))
        .context("set ANTHROPIC_API_KEY or CLAUDE_API_KEY to run the reportlog smoke agent")?;
    let model = std::env::var("RUNIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string());
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());

    let visit_date =
        std::env::var("RUNIC_VISIT_DATE").unwrap_or_else(|_| DEFAULT_VISIT_DATE.to_string());
    let tc_name = std::env::var("RUNIC_TC_NAME").unwrap_or_else(|_| "TC demo".to_string());
    let typology = std::env::var("RUNIC_TYPOLOGY").unwrap_or_else(|_| "Inconnu".to_string());
    let farm_name = std::env::var("RUNIC_FARM_NAME").unwrap_or_else(|_| "Ferme demo".to_string());
    let skills = load_reportlog_skills(&typology)?;

    let provider = Arc::new(AnthropicDriver::new(api_key, base_url));
    let assembly = Assembly {
        provider,
        model,
        instructions: reportlog_system_prompt(
            &visit_date,
            &tc_name,
            &typology,
            &farm_name,
            &skills,
        ),
        memory: None,
        skills: None,
        subagents: None,
        subagent_builder: None,
        mcp: None,
        sessions: None,
        tools: None,
        custom_tools: vec![Arc::new(ResolveDateTool), Arc::new(DaysBetweenTool)],
        output_schema: Some(report_schema()),
        max_turns: Some(8),
        write_hooks: vec![],
    };
    let mut agent = assemble(&assembly, "local-user", "reportlog-smoke").await;

    let outcome = agent
        .run_with(
            transcript,
            RunContext::new().config_value("visit_date", json!(visit_date)),
        )
        .await?;

    if let Some(report) = outcome.structured {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if let Some(answer) = agent.state().last_assistant_text() {
        println!("{answer}");
    }

    Ok(())
}

fn reportlog_system_prompt(
    visit_date: &str,
    tc_name: &str,
    typology: &str,
    farm_name: &str,
    skills: &str,
) -> String {
    format!(
        r#"# ROLE - TU ES LE TC QUI RENTRE DE VISITE

Tu rediges ton propre compte-rendu apres une visite terrain. L'audio est en francais et ta sortie est en francais.

## PARAMETRES DE REFERENCE

* DATE_VISITE: {visit_date} - reference pour toute expression temporelle relative.
* EXPLOITATION: {farm_name}
* TC: {tc_name} (toi)
* TYPOLOGIE: {typology} - Vert / Bleu / Jaune / Rouge / Inconnu.

## REGLES DU HARNESS REPORTLOG

- `resume` est impersonnel, 2 a 3 lignes maximum, sans "je".
- `watch`, `next_visit_angles` et `tasks` sont en premiere personne TC.
- N'invente jamais de faits absents de la transcription.
- La typologie shape la selection, l'ordre et la formulation, mais ne fabrique pas d'observations.
- Appelle `resolve_date` pour chaque deadline explicite avant de remplir `due_date`.
- Une `due_date` vient seulement d'une consigne explicite ("avant vendredi", "d'ici mercredi", "pour le 12 mai").
- Si la date est vague, mets `null`.
- `administratif` est reserve a la paperasse reglementaire; preparation commerciale = `commercial`.
- Bannis les acronymes; ecris en clair.

## SKILLS CHARGES

Ces skills viennent du harness reportlog et remplacent les lectures `/skills/...`
du Deep Agent original dans ce smoke runner Runic.

{skills}

Retourne uniquement l'appel structure final conforme au schema `final_answer`.
"#
    )
}

fn load_reportlog_skills(typology: &str) -> Result<String> {
    let root = std::env::var("RUNIC_REPORTLOG_SKILLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_REPORTLOG_SKILLS_DIR));
    let task_priority = read_skill(&root.join("task-priority").join("SKILL.md"))?;
    let typology_profile = read_skill(
        &root
            .join("typology-profile")
            .join(typology_skill_file(typology)),
    )?;

    Ok(format!(
        "### /skills/task-priority/SKILL.md\n\n{task_priority}\n\n### /skills/typology-profile/{}\n\n{typology_profile}",
        typology_skill_file(typology)
    ))
}

fn read_skill(path: &Path) -> Result<String> {
    if !path.exists() {
        bail!(
            "missing reportlog skill file: {} (set RUNIC_REPORTLOG_SKILLS_DIR to override)",
            path.display()
        );
    }
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

fn typology_skill_file(typology: &str) -> &'static str {
    match typology.trim().to_lowercase().as_str() {
        "vert" | "green" => "green.md",
        "bleu" | "blue" => "blue.md",
        "jaune" | "yellow" => "yellow.md",
        "rouge" | "red" => "red.md",
        _ => "unknown.md",
    }
}

fn report_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "resume": { "type": "string" },
            "watch": {
                "type": "object",
                "properties": {
                    "technique": { "type": "array", "items": { "type": "string" } },
                    "commercial": { "type": "array", "items": { "type": "string" } },
                    "relationnel": { "type": "array", "items": { "type": "string" } },
                    "autre": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["technique", "commercial", "relationnel", "autre"]
            },
            "next_visit_angles": {
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "maxItems": 4
            },
            "tasks": {
                "anyOf": [
                    { "type": "null" },
                    {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "content": { "type": "string" },
                            "category": {
                                "type": "string",
                                "enum": ["commercial", "technique", "administratif", "relationnel", "autre"]
                            },
                            "urgency": {
                                "type": "string",
                                "enum": ["low", "medium", "high"]
                            },
                            "due_date": {
                                "anyOf": [
                                    { "type": "string", "pattern": "^\\d{4}-\\d{2}-\\d{2}$" },
                                    { "type": "null" }
                                ]
                            },
                            "items": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["title", "content", "category", "urgency", "due_date", "items"]
                    }
                ]
            }
        },
        "required": ["resume", "watch", "next_visit_angles", "tasks"]
    })
}

struct ResolveDateTool;

#[async_trait]
impl Tool for ResolveDateTool {
    fn name(&self) -> &str {
        "resolve_date"
    }

    fn description(&self) -> &str {
        "Convertit une expression temporelle francaise simple en date ISO YYYY-MM-DD, ancree sur la date de visite."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "expression": { "type": "string", "description": "Expression temporelle en francais." }
            },
            "required": ["expression"]
        })
    }

    fn parallelizable(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(expression) = args.get("expression").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("resolve_date requires `expression`"));
        };
        let visit_date = ctx
            .config("visit_date")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_VISIT_DATE);
        Ok(match resolve_date(expression, visit_date) {
            Some(date) => ToolResult::ok(format!("{} ({})", date, weekday_fr(date))),
            None => ToolResult::error(format!(
                "Impossible de parser '{expression}'. Laisse la date vide si echec."
            )),
        })
    }
}

struct DaysBetweenTool;

#[async_trait]
impl Tool for DaysBetweenTool {
    fn name(&self) -> &str {
        "days_between"
    }

    fn description(&self) -> &str {
        "Calcule le nombre de jours entre deux dates ISO YYYY-MM-DD."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "start": { "type": "string", "description": "Date de depart YYYY-MM-DD." },
                "end": { "type": "string", "description": "Date d'arrivee YYYY-MM-DD." }
            },
            "required": ["start", "end"]
        })
    }

    fn parallelizable(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(start) = args.get("start").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("days_between requires `start`"));
        };
        let Some(end) = args.get("end").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("days_between requires `end`"));
        };
        let Ok(start) = NaiveDate::parse_from_str(start, "%Y-%m-%d") else {
            return Ok(ToolResult::error("invalid `start`, expected YYYY-MM-DD"));
        };
        let Ok(end) = NaiveDate::parse_from_str(end, "%Y-%m-%d") else {
            return Ok(ToolResult::error("invalid `end`, expected YYYY-MM-DD"));
        };
        Ok(ToolResult::ok(format!(
            "{} jours",
            (end - start).num_days()
        )))
    }
}

fn resolve_date(expression: &str, visit_date: &str) -> Option<NaiveDate> {
    let expr = normalize(expression);
    let reference = NaiveDate::parse_from_str(visit_date, "%Y-%m-%d").ok()?;

    if let Ok(date) = NaiveDate::parse_from_str(&expr, "%Y-%m-%d") {
        return Some(date);
    }
    if let Ok(date) = NaiveDate::parse_from_str(&format!("{expr}/{}", reference.year()), "%d/%m/%Y")
    {
        return Some(adjust_future(date, reference));
    }

    match expr.as_str() {
        "aujourd'hui" | "aujourdhui" => return Some(reference),
        "demain" => return Some(reference + Duration::days(1)),
        "apres-demain" | "apres demain" => return Some(reference + Duration::days(2)),
        "fin du mois" => return last_day_of_month(reference.year(), reference.month()),
        "debut du mois" | "début du mois" => {
            return NaiveDate::from_ymd_opt(reference.year(), reference.month(), 1);
        }
        _ => {}
    }

    if let Some(days) = expr
        .strip_prefix("dans ")
        .and_then(|s| s.strip_suffix(" jours"))
        .and_then(|s| s.parse::<i64>().ok())
    {
        return Some(reference + Duration::days(days));
    }

    for (idx, name) in WEEKDAYS_FR.iter().enumerate() {
        if expr == *name
            || expr == format!("{name} prochain")
            || expr == format!("avant {name}")
            || expr == format!("d'ici {name}")
            || expr == format!("pour {name}")
        {
            return Some(next_weekday(reference, idx as u32));
        }
    }

    None
}

fn normalize(s: &str) -> String {
    s.trim().to_lowercase().replace('’', "'")
}

const WEEKDAYS_FR: [&str; 7] = [
    "lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi", "dimanche",
];

fn weekday_fr(date: NaiveDate) -> &'static str {
    WEEKDAYS_FR[date.weekday().num_days_from_monday() as usize]
}

fn next_weekday(reference: NaiveDate, weekday_from_monday: u32) -> NaiveDate {
    let today = reference.weekday().num_days_from_monday();
    let delta = (weekday_from_monday + 7 - today) % 7;
    reference + Duration::days(if delta == 0 { 7 } else { delta as i64 })
}

fn adjust_future(date: NaiveDate, reference: NaiveDate) -> NaiveDate {
    if date < reference {
        date.with_year(date.year() + 1).unwrap_or(date)
    } else {
        date
    }
}

fn last_day_of_month(year: i32, month: u32) -> Option<NaiveDate> {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_year, next_month, 1).map(|d| d - Duration::days(1))
}
