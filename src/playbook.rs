//! The human side of the PLAYBOOK approval gate.
//! `looop playbook [diff|approve|reject]` — a change proposed by the AI sits in
//! PLAYBOOK.proposed.md (inert) until acted on here.

use crate::paths::Paths;
use crate::seed;
use anyhow::Result;
use std::fs;
use std::process::{Command, ExitCode};

pub fn cmd_playbook(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    seed::ensure_dirs(paths)?;
    let action = args.first().map(String::as_str).unwrap_or("diff");
    let proposed = paths.playbook_proposed();

    match action {
        "diff" => {
            if !proposed.is_file() {
                println!("looop playbook: no pending change.");
                return Ok(ExitCode::SUCCESS);
            }
            println!("looop playbook — pending change (approved → proposed):");
            let approved = paths.playbook_approved();
            let left = if approved.is_file() {
                approved
            } else {
                paths.playbook()
            };
            let _ = Command::new("diff")
                .args([
                    "-u",
                    "-L",
                    "PLAYBOOK.md (approved)",
                    "-L",
                    "PLAYBOOK.proposed.md",
                ])
                .arg(&left)
                .arg(&proposed)
                .status();
            println!();
            println!("  approve:  looop playbook approve    reject:  looop playbook reject");
        }
        "approve" => {
            if !proposed.is_file() {
                println!("looop playbook: nothing to approve.");
                return Ok(ExitCode::SUCCESS);
            }
            fs::copy(&proposed, paths.playbook())?;
            fs::copy(paths.playbook(), paths.playbook_approved())?;
            fs::remove_file(&proposed)?;
            println!("looop playbook: approved — PLAYBOOK.md updated (takes effect next tick).");
        }
        "reject" => {
            if !proposed.is_file() {
                println!("looop playbook: nothing to reject.");
                return Ok(ExitCode::SUCCESS);
            }
            fs::remove_file(&proposed)?;
            println!("looop playbook: rejected — proposal discarded; PLAYBOOK.md unchanged.");
        }
        _ => {
            eprintln!("usage: looop playbook [diff|approve|reject]");
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}
