//! What the recording needs from a .vpx file's script: the ROM name (when
//! Popper's ROM field is empty) and the trough switches. Trough parsing is
//! ported from VRTableManager's `RomAudio.TroughSwitchesIn` (MIT).

use anyhow::{Context, Result};
use std::collections::HashMap;
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

/// A script line without its comment (`'` outside a string, to the end),
/// trimmed: `Controller.Switch(74) = 1 'Controller.Switch(55) = 1` is
/// switch 74 only.
fn code(raw: &str) -> &str {
    if is_rem(raw.trim_start()) {
        return "";
    }
    let mut in_str = false;
    for (i, c) in raw.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '\'' if !in_str => return raw[..i].trim(),
            ':' if !in_str && is_rem(raw[i + 1..].trim_start()) => return raw[..i].trim(),
            _ => {}
        }
    }
    raw.trim()
}

fn is_comment(line: &str) -> bool {
    line.starts_with('\'') || is_rem(line)
}

/// `Rem` followed by whitespace or the end of the line.
fn is_rem(stmt: &str) -> bool {
    starts_ci(stmt, "rem") && stmt[3..].chars().next().is_none_or(char::is_whitespace)
}

/// The sub a `_Hit` line belongs to is the drain or outhole
/// (`Sub Drain_Hit`, `Sub sw55_Hit(): ... RandomSoundDrain`): a ball there
/// at the start is a drained ball, not one in the trough. Only the sub's
/// name and a drain sound call on a single-switch line count.
fn is_drain_sub(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let name = lower.split("_hit").next().unwrap_or("");
    if name.contains("drain") || name.contains("outhole") {
        return true;
    }
    // `RandomSoundDrain swNN` names the drain kicker itself.
    lower.contains("sounddrain")
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
        let line = code(raw);
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
/// `Const` values in the script (`Const swTrough1=11, swTrough2=12`), by
/// lower-case name, so switch lists written with names can be read.
fn consts(script: &str) -> HashMap<String, i32> {
    let mut out = HashMap::new();
    for raw in script.lines() {
        let line = code(raw);
        let Some(rest) = after_ci(line, "const ").filter(|_| starts_ci(line, "const ")) else {
            continue;
        };
        let rest = rest.split('\'').next().unwrap_or("");
        for part in rest.split([',', ':']) {
            if let Some((name, value)) = part.split_once('=') {
                if let Ok(v) = value.trim().parse::<i32>() {
                    out.insert(name.trim().to_ascii_lowercase(), v);
                }
            }
        }
    }
    out
}

/// A comma-separated list of switch numbers or `Const` names, up to the
/// closing bracket or a comment. Anything else counts as 0.
fn int_list(s: &str, consts: &HashMap<String, i32>) -> Vec<i32> {
    let list = s.split([')', '\'', ':']).next().unwrap_or("");
    list.split(',')
        .map(|x| {
            let x = x.trim();
            x.parse()
                .ok()
                .or_else(|| consts.get(&x.to_ascii_lowercase()).copied())
                .unwrap_or(0)
        })
        .collect()
}

/// An integer expression as scripts write DIP settings: sums of products
/// of numbers (`0*1 + 1*2 + 1*128`), `&H` hex allowed. None if it's
/// anything else.
fn eval_sum(expr: &str) -> Option<i64> {
    let e: String = expr.chars().filter(|c| !matches!(c, '(' | ')')).collect();
    let num = |t: &str| {
        let t = t.trim();
        match t.get(..2) {
            Some(h) if h.eq_ignore_ascii_case("&h") => i64::from_str_radix(&t[2..], 16).ok(),
            _ => t.parse::<i64>().ok(),
        }
    };
    e.split('+')
        .map(|term| term.split('*').map(num).product::<Option<i64>>())
        .sum()
}

/// DIP switch banks the script sets: `Controller.Dip(0) = (0*1 + 1*2 ...)`.
/// Some machines (Bally 6803, Special Force) won't start a game without
/// them.
pub fn dip_switches(script: &str) -> Vec<(i32, i64)> {
    let mut out: Vec<(i32, i64)> = Vec::new();
    for raw in script.lines() {
        let line = code(raw);
        if is_comment(line) {
            continue;
        }
        let Some(rest) = after_ci(line, "controller.dip") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('(') else {
            continue;
        };
        let Some((bank, rest)) = rest.split_once(')') else {
            continue;
        };
        let Ok(bank) = bank.trim().parse::<i32>() else {
            continue;
        };
        let Some(expr) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let expr = expr.split(['\'', ':']).next().unwrap_or("");
        if let Some(v) = eval_sum(expr).filter(|v| (0..=0xFFFF_FFFF).contains(v)) {
            out.retain(|(b, _)| *b != bank);
            out.push((bank, v));
        }
    }
    out
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
    let consts = consts(script);
    let mut found = Vec::new();
    let mut drains = Vec::new();
    let mut in_trough_with = false;
    for raw in script.lines() {
        let line = code(raw);
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
                    found.extend(int_list(rest, &consts));
                    continue;
                }
            }
        }
        if let Some(rest) = after_ci(line, ".initsw") {
            if rest.starts_with(|c: char| c.is_whitespace() || c == '(') {
                found.extend(
                    int_list(rest.trim_start().trim_start_matches('('), &consts)
                        .into_iter()
                        .skip(1),
                );
                continue;
            }
        }
        // Not the drain/outhole: a ball there at the start is a drained
        // ball (Genesis: `Sub sw55_Hit(): Controller.Switch(55) = 1 :
        // RandomSoundDrain sw55 : UpdateTrough`), and the game won't start.
        if contains_ci(line, "_hit") {
            if is_drain_sub(line) {
                drains.extend(switches_on(line));
            } else {
                found.extend(switches_on(line));
            }
        }
    }
    if found.is_empty() {
        // A single-ball machine whose ball rests in the outhole.
        found = drains;
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

/// Switches of mechanisms the script simulates with `cvpmMech` (The Addams
/// Family's Thing hand, Bride of Pin-bot's head...) that are closed at the
/// starting position: `.AddSw 84, 0, 1` (closed from step 0). Nobody closes
/// them without the script, so the ROM thinks the mechanism is stuck and
/// won't start a game.
pub fn mech_rest_switches(script: &str) -> Vec<i32> {
    let consts = consts(script);
    let mut out = Vec::new();
    for raw in script.lines() {
        let line = code(raw);
        if is_comment(line) {
            continue;
        }
        let Some(rest) = after_ci(line, ".addsw") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '(') {
            continue;
        }
        let n = int_list(rest.trim_start().trim_start_matches('('), &consts);
        if let [sw, 0, _, ..] = n[..] {
            if sw > 0 && !out.contains(&sw) {
                out.push(sw);
            }
        }
    }
    out.truncate(8);
    out
}

/// The core script a table loads: `LoadVPM "01500000", "6803.VBS", 3.2`
/// gives `6803.VBS`. Only a bare file name is returned.
pub fn core_script_name(script: &str) -> Option<String> {
    for raw in script.lines() {
        let line = raw.split('\'').next().unwrap_or("").trim();
        let Some(rest) = after_ci(line, "loadvpm") else {
            continue;
        };
        let mut quoted = rest.split('"').skip(1).step_by(2);
        let _version = quoted.next();
        if let Some(name) = quoted.next() {
            let name = name.trim();
            let ok = !name.is_empty()
                && name.len() < 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                && name.to_ascii_lowercase().ends_with(".vbs");
            if ok {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// The coin and Start switches a core script names (`swCoin1`,
/// `swStartButton`).
pub fn cabinet_switches(core: &str) -> crate::pinmame::CabinetSwitches {
    let c = consts(core);
    crate::pinmame::CabinetSwitches {
        coin: c.get("swcoin1").copied(),
        start: c.get("swstartbutton").copied(),
    }
}

/// A mechanism the script hands to PinMAME (`Set m = New cvpmMech` ...
/// `.Start`, which writes `Controller.Mech`): the Addams Family's Thing,
/// Starship Troopers' warrior bug. PinMAME moves it while its solenoid
/// runs and works its position switches, as it does under VPX.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScriptMech {
    pub mtype: i32,
    pub sol1: i32,
    pub sol2: i32,
    pub length: i32,
    pub steps: i32,
    pub acc: i32,
    pub ret: i32,
    /// (switch, start, end, pulse)
    pub switches: Vec<(i32, i32, i32, i32)>,
}

impl ScriptMech {
    /// As the worker's `--mech` value: `type,sol1,sol2,length,steps,acc,ret`
    /// then `,sw:start:end:pulse` per switch.
    pub fn to_arg(&self) -> String {
        let mut s = format!(
            "{},{},{},{},{},{},{}",
            self.mtype, self.sol1, self.sol2, self.length, self.steps, self.acc, self.ret
        );
        for (sw, a, b, p) in &self.switches {
            s.push_str(&format!(",{sw}:{a}:{b}:{p}"));
        }
        s
    }

    pub fn parse_arg(arg: &str) -> Option<ScriptMech> {
        let mut parts = arg.split(',');
        let mut n = || parts.next()?.trim().parse::<i32>().ok();
        let mut m = ScriptMech {
            mtype: n()?,
            sol1: n()?,
            sol2: n()?,
            length: n()?,
            steps: n()?,
            acc: n()?,
            ret: n()?,
            switches: Vec::new(),
        };
        for sw in arg.split(',').skip(7) {
            let v: Vec<i32> = sw
                .split(':')
                .filter_map(|x| x.trim().parse().ok())
                .collect();
            if let [a, b, c, d] = v[..] {
                m.switches.push((a, b, c, d));
            } else {
                return None;
            }
        }
        Some(m)
    }
}

/// core.vbs's mechanism flags.
fn mech_const(name: &str) -> Option<i64> {
    Some(match name {
        "vpmmechlinear" | "vpmmechcircle" | "vpmmechonesol" | "vpmmechslow" | "vpmmechstepsw" => 0,
        "vpmmechnonlinear" => 0x01,
        "vpmmechstopend" => 0x02,
        "vpmmechreverse" => 0x04,
        "vpmmechonedirsol" => 0x10,
        "vpmmechtwodirsol" => 0x20,
        "vpmmechstepsol" | "vpmmechtwostepsol" => 0x40,
        "vpmmechfourstepsol" => 0x60,
        "vpmmechfast" => 0x80,
        "vpmmechlengthsw" => 0x100,
        _ => return None,
    })
}

/// `a + b + &H10 + NAME` with names from the script's consts or core.vbs.
fn eval_terms(expr: &str, consts: &HashMap<String, i32>) -> Option<i64> {
    let e: String = expr.chars().filter(|c| !matches!(c, '(' | ')')).collect();
    let mut total = 0i64;
    for term in e.split('+') {
        let t = term.trim();
        let (neg, t) = match t.strip_prefix('-') {
            Some(r) => (true, r.trim()),
            None => (false, t),
        };
        let v = if t.get(..2).is_some_and(|h| h.eq_ignore_ascii_case("&h")) {
            i64::from_str_radix(t[2..].trim_end_matches('&'), 16).ok()?
        } else if let Ok(v) = t.parse::<i64>() {
            v
        } else {
            let lower = t.to_ascii_lowercase();
            mech_const(&lower).or_else(|| consts.get(&lower).map(|&v| v as i64))?
        };
        total += if neg { -v } else { v };
    }
    Some(total)
}

/// Statements of a script line (split at `:` outside strings).
fn statements(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut from, mut in_str) = (0, false);
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            ':' if !in_str => {
                out.push(line[from..i].trim());
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push(line[from..].trim());
    out
}

/// The mechanisms the script starts, in creation order (PinMAME mech 1, 2...).
pub fn mechs(script: &str) -> Vec<ScriptMech> {
    let consts = consts(script);
    let mut names: Vec<String> = Vec::new();
    let mut found: Vec<(ScriptMech, bool)> = Vec::new();
    let mut with: Option<usize> = None;
    for raw in script.lines() {
        for st in statements(code(raw)) {
            let lower = st.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("set ") {
                if let Some((var, rhs)) = rest.split_once('=') {
                    let rhs = rhs.trim();
                    if let Some(cls) = rhs.strip_prefix("new ") {
                        let cls = cls.trim();
                        if cls.starts_with("cvpm") && cls.ends_with("mech") {
                            names.push(var.trim().to_string());
                            found.push((ScriptMech::default(), false));
                        }
                    }
                }
                continue;
            }
            if let Some(var) = lower.strip_prefix("with ") {
                with = names.iter().position(|n| n == var.trim());
                continue;
            }
            if lower.starts_with("end with") {
                with = None;
                continue;
            }
            // `.Prop = v` inside With, or `name.Prop = v`.
            let (idx, member) = if let Some(m) = lower.strip_prefix('.') {
                match with {
                    Some(i) => (i, m),
                    None => continue,
                }
            } else if let Some((var, m)) = lower.split_once('.') {
                match names.iter().position(|n| n == var.trim()) {
                    Some(i) => (i, m),
                    None => continue,
                }
            } else {
                continue;
            };
            let m = &mut found[idx];
            let member = member.trim();
            if member == "start" || member.starts_with("start ") || member.starts_with("start(") {
                m.1 = true;
                continue;
            }
            let call = |name: &str| -> Option<Vec<i32>> {
                let rest = member.strip_prefix(name)?;
                if !rest.starts_with(|c: char| c.is_whitespace() || c == '(') {
                    return None;
                }
                Some(
                    rest.trim_start()
                        .trim_start_matches('(')
                        .split(',')
                        .map(|x| {
                            eval_terms(x.trim().trim_end_matches(')'), &consts).unwrap_or(0) as i32
                        })
                        .collect(),
                )
            };
            if let Some(v) = call("addsw") {
                if let [sw, a, b, ..] = v[..] {
                    m.0.switches.push((sw, a, b, 0));
                }
                continue;
            }
            if let Some(v) = call("addpulseswnew") {
                if let [sw, interval, a, b, ..] = v[..] {
                    m.0.switches.push((sw, a, b, interval));
                }
                continue;
            }
            let Some((prop, value)) = member.split_once('=') else {
                continue;
            };
            let Some(v) = eval_terms(value, &consts) else {
                continue;
            };
            let v = v as i32;
            match prop.trim() {
                "sol1" => m.0.sol1 = v,
                "sol2" => m.0.sol2 = v,
                "mtype" => m.0.mtype = v,
                "length" => m.0.length = v,
                "steps" => m.0.steps = v,
                "acc" => m.0.acc = v,
                "ret" => m.0.ret = v,
                _ => {}
            }
        }
    }
    found
        .into_iter()
        .filter(|(m, started)| *started && m.sol1 > 0 && m.steps > 0)
        .map(|(m, _)| m)
        .collect()
}

/// `.HandleMechanics = N` in the script, when it lets PinMAME simulate a
/// machine's own mechanisms (Monster Bash: 4).
pub fn handle_mechanics(script: &str) -> Option<i32> {
    let mut out = None;
    for raw in script.lines() {
        for st in statements(code(raw)) {
            if let Some(rest) = after_ci(st, "handlemechanics") {
                if let Some(v) = rest.trim_start().strip_prefix('=') {
                    if let Ok(n) = v.trim().parse::<i32>() {
                        out = Some(n);
                    }
                }
            }
        }
    }
    out
}

/// Switches the table's init subs set (`Controller.Switch(9) = 1`, or
/// `.Switch(22) = 1` inside `With Controller`): (closed, opened). These are
/// the switch states the table starts a game from under VPX: balls in the
/// trough, a T-rex at rest, a coin door closed, a switch wired backwards.
pub fn init_switches(script: &str) -> (Vec<i32>, Vec<i32>) {
    let (mut on, mut off) = (Vec::new(), Vec::new());
    let mut in_init = false;
    for raw in script.lines() {
        let line = code(raw);
        let lower = line.to_ascii_lowercase();
        let head = lower
            .trim_start_matches("private ")
            .trim_start_matches("public ");
        if let Some(name) = head.strip_prefix("sub ") {
            let name = name.split(['(', ':', ' ']).next().unwrap_or("");
            in_init = name.contains("init");
        }
        if !in_init {
            continue;
        }
        for st in statements(line) {
            let l = st.to_ascii_lowercase();
            let Some(at) = l.find(".switch(") else {
                continue;
            };
            let rest = &l[at + ".switch(".len()..];
            let Some((n, rest)) = rest.split_once(')') else {
                continue;
            };
            let Ok(n) = n.trim().parse::<i32>() else {
                continue;
            };
            let Some(v) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let v = v.trim();
            let state = if v == "1" || v == "true" {
                true
            } else if v == "0" || v == "false" {
                false
            } else {
                continue;
            };
            on.retain(|&x| x != n);
            off.retain(|&x| x != n);
            if state {
                on.push(n);
            } else {
                off.push(n);
            }
        }
        if lower.starts_with("end sub") || lower.ends_with("end sub") {
            in_init = false;
        }
    }
    (on, off)
}

/// The switches closed when a recording starts: balls in the trough, what
/// the table's init closes, and mechanisms at rest. Switches the init opens
/// are left open.
pub fn start_switches(script: &str) -> Vec<i32> {
    let mut out = trough_switches(script);
    let (on, off) = init_switches(script);
    for sw in on {
        if !out.contains(&sw) {
            out.push(sw);
        }
    }
    for sw in mech_rest_switches(script) {
        if !out.contains(&sw) {
            out.push(sw);
        }
    }
    // Rest switches stay closed even for mechanisms PinMAME simulates: it
    // doesn't close them until the mechanism first moves (The Addams
    // Family won't start without 82 and 84).
    out.retain(|sw| *sw > 0 && !off.contains(sw));
    out.truncate(16);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warrior_bug_mech() {
        let s = "Set mBug = New cvpmMyMech\n\tWith mBug\n\t\t.MType=vpmMechStepSol+vpmMechTwoDirSol+vpmMechStopEnd+vpmMechLinear+vpmMechFast\n\t\t.Sol1=17\n\t\t.Length=600 '150\n\t\t.Steps=222\n\t\t.AddSw 34,0,1 'start\n\t\t.AddSw 33,221,222\n\t\t.Callback=GetRef(\"UpdateWarriorBug\")\n\t\t.Start\n\tEnd With\nSet mNot = New cvpmMech\nmNot.Sol1 = 5 : mNot.Steps = 10\n";
        let m = mechs(s);
        assert_eq!(m.len(), 1, "a mech never started doesn't count");
        assert_eq!(m[0].mtype, 0xE2);
        assert_eq!((m[0].sol1, m[0].length, m[0].steps), (17, 600, 222));
        assert_eq!(m[0].switches, vec![(34, 0, 1, 0), (33, 221, 222, 0)]);
        assert_eq!(ScriptMech::parse_arg(&m[0].to_arg()).as_ref(), Some(&m[0]));
        let t = "Table1_Init\nSub Table1_Init\n  Controller.Switch(14) = 1 : Controller.Switch(31) = 0\n  With Controller\n   .Switch(22) = 1 'door\n  .HandleMechanics = 4\n  End With\n  Init_Trex\nEnd Sub\nSub Init_Trex()\n controller.switch(36) = True\n controller.switch(58) = False\nEnd Sub\nSub sw31_Hit: Controller.Switch(31) = 1 : End Sub\n";
        assert_eq!(init_switches(t), (vec![14, 22, 36], vec![31, 58]));
        assert_eq!(handle_mechanics(t), Some(4));
        let both = format!("{s}{t}");
        let sw = start_switches(&both);
        assert!(sw.contains(&14) && sw.contains(&36) && !sw.contains(&31) && sw.contains(&34));
    }

    #[test]
    fn commented_switches_are_ignored() {
        let s = "Sub sw74_Hit: Controller.Switch(74) = 1 : bsTrough.AddBall Me ' Controller.Switch(55) = 1\n\
                 bsTrough.InitSw 0, 74, 0, 0, 0, 0, 0, 0 'bsTrough.InitSw 0, 55\n";
        assert_eq!(trough_switches(s), vec![74]);
        assert_eq!(code("x = \"it's\" ' note"), "x = \"it's\"");
        let g = "Sub sw74_Hit():Controller.Switch(74) = 1 : UpdateTrough : End Sub\n\
                 Sub sw55_Hit():Controller.Switch(55) = 1 : RandomSoundDrain sw55 : UpdateTrough : End Sub\n";
        assert_eq!(trough_switches(g), vec![74]);
        let one = "Sub sw55_Hit():Controller.Switch(55) = 1 : RandomSoundDrain sw55 : UpdateTrough : End Sub\n";
        assert_eq!(
            trough_switches(one),
            vec![55],
            "the outhole when it's all there is"
        );
        assert_eq!(
            code("Controller.Switch(74) = 1 : Rem Controller.Switch(55) = 1"),
            "Controller.Switch(74) = 1"
        );
        assert_eq!(code("REM it's off"), "");
        assert_eq!(code("Remove = 1"), "Remove = 1");
    }

    #[test]
    fn core_script_and_its_switches() {
        let s = "' LoadVPM \"x\", \"old.vbs\"\nLoadVPM \"01500000\", \"6803.VBS\", 3.2\n";
        assert_eq!(core_script_name(s).as_deref(), Some("6803.VBS"));
        assert_eq!(core_script_name("LoadVPM \"01\", \"..\\x.vbs\", 1"), None);
        let core = "Const swStartButton = 6, swCoin1 = 10\nConst swCPUDiag=-7\n";
        let c = cabinet_switches(core);
        assert_eq!((c.coin, c.start), (Some(10), Some(6)));
    }

    #[test]
    fn mech_switches_closed_at_rest() {
        let s = "Set mHand = New cvpmMech\nWith mHand\n .Sol1 = 27\n .AddSw 84, 0, 1\n .AddSw 83, 40, 41\nEnd With\n' .AddSw 99, 0, 1\nmBook.AddSw(82, 0, 3)\nmX.AddSwitch 5, 0, 1\n";
        assert_eq!(mech_rest_switches(s), vec![84, 82]);
        let t = "bsTrough.InitSw 0, 15, 16, 0, 0, 0, 0, 0\n";
        assert_eq!(start_switches(&format!("{t}{s}")), vec![15, 16, 84, 82]);
    }

    #[test]
    fn switch_lists_by_const_name() {
        let s = "Const swOuthole=9,swUpDownRamp=10,swTrough1=11,swTrough2=12,swTrough3=13\n\
                 Const swClockWheel = 59 ' clock\n\
                 Set bsTrough = New cvpmTrough\nWith bsTrough\n .initSwitches Array(swTrough1,swTrough2,swTrough3)\nEnd With\n\
                 With WheelMech\n .AddSw swClockWheel, 0, 0\nEnd With\n";
        assert_eq!(start_switches(s), vec![11, 12, 13, 59]);
    }

    #[test]
    fn dip_settings() {
        let s =
            "  Controller.Dip(0) = (0*1 + 1*2 + 0*4 + 0*8 + 0*16 + 0*32 + 0*64 + 0*128) '01-08\n\
                 Controller.Dip(1) = (0*1 + 0*2 + 0*4 + 1*8 + 1*16 + 1*32 + 1*64 + 1*128) '09-16\n\
                 'Controller.Dip(2) = 255\n\
                 Controller.Dip(3) = &H67\n\
                 Controller.Dip(4) = x\n";
        assert_eq!(dip_switches(s), vec![(0, 2), (1, 248), (3, 103)]);
    }

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
