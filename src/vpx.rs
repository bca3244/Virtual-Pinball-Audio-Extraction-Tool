//! What the recording needs from a .vpx file's script: the ROM name (when
//! Popper's ROM field is empty) and the trough switches. Trough parsing is
//! ported from VRTableManager's `RomAudio.TroughSwitchesIn` (MIT).

use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

/// The table script: the `CODE` record of `GameStg/GameData`.
pub fn read_script(vpx: &Path) -> Result<String> {
    let mut cf = cfb::open(vpx).with_context(|| format!("opening {}", vpx.display()))?;
    let mut stream = cf
        .open_stream("/GameStg/GameData")
        .context("no GameStg/GameData stream")?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    Ok(script_from_game_data(&bytes))
}

/// Walks the BIFF-style records: `[len: i32][tag: 4 bytes][data]`, where
/// `len` covers the tag and data. CODE is the exception: a second length
/// follows the tag and the script follows that.
pub fn script_from_game_data(bytes: &[u8]) -> String {
    let int = |at: usize| -> Option<i32> {
        bytes
            .get(at..at + 4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let mut i = 0usize;
    while i + 8 <= bytes.len() {
        let Some(len) = int(i) else { break };
        if len < 4 || i + 4 + len as usize > bytes.len() {
            break;
        }
        if &bytes[i + 4..i + 8] == b"CODE" {
            let code_len = int(i + 8).unwrap_or(0).max(0) as usize;
            let start = i + 12;
            let end = (start + code_len).min(bytes.len());
            // Scripts are ANSI (Latin-1 is a safe superset for parsing).
            return bytes
                .get(start..end)
                .unwrap_or_default()
                .iter()
                .map(|&b| b as char)
                .collect();
        }
        i += 4 + len as usize;
    }
    String::new()
}

fn is_comment(line: &str) -> bool {
    line.starts_with('\'') || starts_ci(line, "rem ")
}

fn starts_ci(line: &str, prefix: &str) -> bool {
    line.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// The ROM the script loads: `Const cGameName = "afm_113b"` (the
/// convention nearly every ROM table follows).
pub fn rom_name(script: &str) -> Option<String> {
    for raw in script.lines() {
        let line = raw.trim();
        if is_comment(line) {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let Some(at) = lower.find("cgamename") else {
            continue;
        };
        let rest = &line[at + "cgamename".len()..];
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        let name: String = rest.chars().take_while(|&c| c != '"').collect();
        let name = name.trim().to_ascii_lowercase();
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Some(name);
        }
    }
    None
}

/// Comma-separated integers at the start of `s` (spaces allowed).
fn int_list(s: &str) -> Vec<i32> {
    let list: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || c.is_whitespace())
        .collect();
    list.split(',')
        .map(|x| x.trim().parse().unwrap_or(0))
        .collect()
}

/// Position just after `pat` in `line` (case-insensitive).
fn after_ci<'a>(line: &'a str, pat: &str) -> Option<&'a str> {
    let at = line.to_ascii_lowercase().find(&pat.to_ascii_lowercase())?;
    Some(&line[at + pat.len()..])
}

/// `Controller.Switch(32) = 1` occurrences.
fn switches_on(line: &str) -> Vec<i32> {
    let mut out = Vec::new();
    let lower = line.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find("controller.switch") {
        let rest = &line[from + at + "controller.switch".len()..];
        from += at + 1;
        let Some(rest) = rest.trim_start().strip_prefix('(') else {
            continue;
        };
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let Ok(n) = digits.parse::<i32>() else {
            continue;
        };
        let rest = rest.trim_start()[digits.len()..].trim_start();
        let Some(rest) = rest.strip_prefix(')') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        if rest.starts_with('1') && !rest[1..].starts_with(|c: char| c.is_ascii_alphanumeric()) {
            out.push(n);
        }
    }
    out
}

/// The switches that see balls resting in the trough: a machine started with
/// an empty trough says "locating pinballs" and plays nothing. Scripts name
/// them as `bsTrough.InitSw 0, 21, 20, 19, 18, ...` (the first is the entry
/// switch), `.InitSwitches Array(32, 33, 34)` inside `With bsTrough`, or
/// `Sub sw32_Hit: Controller.Switch(32) = 1` on trough lines.
pub fn trough_switches(script: &str) -> Vec<i32> {
    let mut found = Vec::new();
    let mut in_trough_with = false;
    for raw in script.lines() {
        let line = raw.trim();
        if starts_ci(line, "with ") {
            in_trough_with = contains_ci(line, "trough");
        } else if starts_ci(line, "end with") {
            in_trough_with = false;
        }
        if is_comment(line) || !(in_trough_with || contains_ci(line, "trough")) {
            continue;
        }
        if let Some(rest) = after_ci(line, ".initswitches") {
            let rest = rest.trim_start().trim_start_matches('(').trim_start();
            if let Some(rest) = after_ci(rest, "array") {
                if let Some(rest) = rest.trim_start().strip_prefix('(') {
                    found.extend(int_list(rest));
                    continue;
                }
            }
        }
        if let Some(rest) = after_ci(line, ".initsw") {
            if rest.starts_with(|c: char| c.is_whitespace() || c == '(') {
                found.extend(
                    int_list(rest.trim_start().trim_start_matches('('))
                        .into_iter()
                        .skip(1),
                );
                continue;
            }
        }
        if contains_ci(line, "_hit") {
            found.extend(switches_on(line));
        }
    }
    let mut out = Vec::new();
    for n in found.into_iter().filter(|&n| n > 0) {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out.truncate(8);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_rom_name() {
        let s = "Option Explicit\r\n' Const cGameName = \"old\"\r\nConst cGameName  =  \"AFM_113b\" ' rom\r\n";
        assert_eq!(rom_name(s).as_deref(), Some("afm_113b"));
        assert_eq!(rom_name("Const cGameName = \"\"\n"), None);
        assert_eq!(rom_name("Dim x\n"), None);
    }

    #[test]
    fn init_sw_skips_entry_switch() {
        let s = "Set bsTrough = New cvpmBallStack\n  bsTrough.InitSw 0, 21, 20, 19, 18, 0, 0, 0\n";
        assert_eq!(trough_switches(s), vec![21, 20, 19, 18]);
    }

    #[test]
    fn init_switches_array_inside_with() {
        let s = "With bsTrough\n  .initSwitches Array(32, 33, 34, 35)\n  .Balls = 4\nEnd With\n";
        assert_eq!(trough_switches(s), vec![32, 33, 34, 35]);
    }

    #[test]
    fn hit_subs_on_trough_lines() {
        let s = "Sub Trough1_Hit: Controller.Switch(16) = 1: UpdateTrough: End Sub\n\
                 Sub Trough2_Hit : Controller.Switch( 17 )=1 : End Sub\n\
                 Sub Trough3_UnHit: Controller.Switch(18) = 0: End Sub\n\
                 Sub sw40_Hit: Controller.Switch(40) = 1: End Sub\n\
                 ' Sub Trough9_Hit: Controller.Switch(99) = 1\n";
        assert_eq!(trough_switches(s), vec![16, 17]);
    }

    #[test]
    fn game_data_code_record() {
        let mut data = Vec::new();
        // a NAME record
        data.extend(8i32.to_le_bytes());
        data.extend(b"NAME");
        data.extend(b"abcd");
        // CODE record: len covers just the tag, a second length follows
        let script = b"Const cGameName = \"tz_92\"";
        data.extend(4i32.to_le_bytes());
        data.extend(b"CODE");
        data.extend((script.len() as i32).to_le_bytes());
        data.extend(script);
        let s = script_from_game_data(&data);
        assert_eq!(rom_name(&s).as_deref(), Some("tz_92"));
    }
}
