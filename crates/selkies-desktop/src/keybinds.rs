//! The keyboard shortcuts the window manager publishes, and the overrides
//! the shell writes back for it.
//!
//! The window manager writes the binds in force to
//! `$XDG_RUNTIME_DIR/selkies-wm/keybinds`, one a line, tab-separated: the
//! chord as its configuration spells it, the chord the configuration gave
//! before any override, the chord's label, and what the bind does in the
//! session's language.  A chord is openbox's key spelling: the modifier
//! prefixes `C-` (Ctrl), `A-` (Alt), `S-` (Shift), and `W-` (Super), then
//! the keysym's name (`W-Left`, `C-A-t`, or `Super_L` alone).  An empty chord
//! is a bind the user turned off.  A line opening with `execute` is a bind of
//! the user's own that runs a command: `execute`, the chord, its label, and
//! the command.
//!
//! The overrides live in `$XDG_CONFIG_HOME/selkies-wm/keybinds`, which the
//! window manager reads again within a second of a change: `rebind`, the
//! configured chord, and the chord replacing it (empty to turn the bind off),
//! or `execute`, a chord, and a command.  The shell edits the file line by
//! line, so a comment, a blank line, or a line it does not understand stays
//! where it was, and replaces the file by a rename so the window manager
//! never reads it half written.
//!
//! A captured chord is spelled the way the window manager's configuration
//! spells one: the modifiers held in the order Ctrl, Alt, Shift, Super, then
//! the name of the key's unshifted keysym, a letter in lower case, so the
//! chord the user pressed and the one the configuration names compare equal.

use std::path::PathBuf;

use xkeysym::Keysym;

use crate::event::Modifiers;

/// A bind the window manager's configuration names, as it is in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    /// The chord in force; empty when the user turned the bind off.
    pub spec: String,
    /// The chord the configuration gives, before any override.
    pub origin: String,
    /// The chord in force as a user reads it (`Super+Left`).
    pub label: String,
    /// What the bind does.
    pub description: String,
}

impl Bind {
    /// Whether an override changed the chord or turned the bind off.
    pub fn changed(&self) -> bool {
        canonical(&self.spec) != canonical(&self.origin)
    }
}

/// A bind of the user's own that runs a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Custom {
    /// The chord.
    pub spec: String,
    /// The chord as a user reads it.
    pub label: String,
    /// The command it runs.
    pub command: String,
}

/// Everything the window manager publishes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Published {
    /// The binds of the configuration, in its order.
    pub binds: Vec<Bind>,
    /// The user's command binds, in the order they were added.
    pub custom: Vec<Custom>,
}

impl Published {
    /// What else holds a chord, by the descriptions of the binds and the
    /// commands of the custom binds using it, the one asked about left out.
    ///
    /// # Arguments
    /// * `spec` - The chord.
    /// * `skip` - The description or command of the row asking.
    pub fn others_on(&self, spec: &str, skip: &str) -> Vec<String> {
        let key = canonical(spec);
        if key.is_empty() {
            return Vec::new();
        }
        let binds = self
            .binds
            .iter()
            .filter(|b| canonical(&b.spec) == key)
            .map(|b| b.description.clone());
        let custom = self
            .custom
            .iter()
            .filter(|c| canonical(&c.spec) == key)
            .map(|c| c.command.clone());
        let mut others: Vec<String> = binds.chain(custom).collect();
        if let Some(own) = others.iter().position(|o| o == skip) {
            others.remove(own);
        }
        others
    }
}

/// Parses the published file; lines of another shape are skipped.
///
/// # Arguments
/// * `text` - The file's text.
pub fn parse(text: &str) -> Published {
    let mut published = Published::default();
    for line in text.lines() {
        let fields: Vec<&str> = line.splitn(4, '\t').map(str::trim).collect();
        let [first, second, third, fourth] = fields[..] else {
            continue;
        };
        if first == "execute" {
            if !second.is_empty() && !fourth.is_empty() {
                published.custom.push(Custom {
                    spec: second.to_string(),
                    label: third.to_string(),
                    command: fourth.to_string(),
                });
            }
        } else if !second.is_empty() && !fourth.is_empty() {
            published.binds.push(Bind {
                spec: first.to_string(),
                origin: second.to_string(),
                label: third.to_string(),
                description: fourth.to_string(),
            });
        }
    }
    published
}

/// The file the window manager publishes its binds in.
fn published_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join("selkies-wm").join("keybinds"))
}

/// Reads the published binds; `None` when nothing is published.
pub fn read_published() -> Option<Published> {
    let text = std::fs::read_to_string(published_path()?).ok()?;
    Some(parse(&text))
}

/// The user's overrides file.
pub fn overrides_path() -> PathBuf {
    selkies_draw::dirs::config_home()
        .join("selkies-wm")
        .join("keybinds")
}

/// Edits the overrides file, creating it when missing, and replaces it in
/// one rename.
///
/// # Arguments
/// * `edit` - Makes the new text from the old.
pub fn edit_overrides(edit: impl FnOnce(&str) -> String) -> std::io::Result<()> {
    let path = overrides_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let new = edit(&old);
    if new == old {
        return Ok(());
    }
    let staged = path.with_extension("new");
    std::fs::write(&staged, new)?;
    std::fs::rename(&staged, &path)
}

/// The tab-separated fields of an overrides line, `None` for a comment or a
/// blank line.
fn fields(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    Some(line.splitn(3, '\t').map(str::trim).collect())
}

/// Joins kept lines and a new one into a file's text, a newline after each.
fn join(mut lines: Vec<String>, added: Option<String>) -> String {
    lines.extend(added);
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// The overrides with a configured bind moved to another chord, or put back
/// where the configuration has it.  Any earlier `rebind` of the bind goes;
/// the new one takes the place of the first, or the end of the file.
///
/// # Arguments
/// * `text` - The overrides file's text.
/// * `origin` - The chord the configuration gives the bind.
/// * `spec` - The new chord, empty to turn the bind off, or `None` to reset
///   it; a chord equal to the origin resets it as well.
pub fn with_rebind(text: &str, origin: &str, spec: Option<&str>) -> String {
    let key = canonical(origin);
    let mut added = spec
        .filter(|spec| spec.is_empty() || canonical(spec) != key)
        .map(|spec| format!("rebind\t{origin}\t{spec}"));
    let mut lines = Vec::new();
    for line in text.lines() {
        let rebinds =
            fields(line).is_some_and(|f| f[0] == "rebind" && f.get(1).is_some_and(|o| canonical(o) == key));
        if !rebinds {
            lines.push(line.to_string());
        } else if let Some(new) = added.take() {
            lines.push(new);
        }
    }
    join(lines, added)
}

/// Whether an overrides line is the `execute` bind of a chord and command.
fn is_execute(line: &str, spec: &str, command: &str) -> bool {
    fields(line).is_some_and(|f| {
        f.first() == Some(&"execute")
            && f.get(1).is_some_and(|s| canonical(s) == canonical(spec))
            && f.get(2) == Some(&command.trim())
    })
}

/// The overrides with a bind added that runs a command; the same bind
/// already there is not doubled.
///
/// # Arguments
/// * `text` - The overrides file's text.
/// * `spec` - The chord.
/// * `command` - The command.
pub fn with_execute(text: &str, spec: &str, command: &str) -> String {
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.iter().any(|line| is_execute(line, spec, command)) {
        return join(lines, None);
    }
    join(lines, Some(format!("execute\t{spec}\t{}", command.trim())))
}

/// The overrides without a bind that runs a command.
///
/// # Arguments
/// * `text` - The overrides file's text.
/// * `spec` - The bind's chord.
/// * `command` - The command it runs.
pub fn without_execute(text: &str, spec: &str, command: &str) -> String {
    let lines = text
        .lines()
        .filter(|line| !is_execute(line, spec, command))
        .map(str::to_string)
        .collect();
    join(lines, None)
}

/// Whether a keysym is a modifier key: a Shift, Control, Lock, Alt, Meta,
/// Super, or Hyper of either side, or one of the ISO level and group
/// shifts and locks (AltGr among them).
pub fn is_modifier(keysym: u32) -> bool {
    (Keysym::Shift_L.raw()..=Keysym::Hyper_R.raw()).contains(&keysym)
        || (Keysym::ISO_Lock.raw()..=Keysym::ISO_Level5_Lock.raw()).contains(&keysym)
}

/// The name a configuration spells a keysym by, as xkbcommon names it: the
/// X name without its `XK_` (`Left`, `F4`, `XF86AudioMute`), a letter in
/// lower case, `Uxxxx` for a Unicode keysym without a name, and the number
/// for anything else.
///
/// # Arguments
/// * `keysym` - The keysym.
pub fn key_name(keysym: u32) -> String {
    if let Some(name) = Keysym::new(keysym).name() {
        let name = name.replacen("XK_", "", 1);
        if name.len() == 1 {
            return name.to_ascii_lowercase();
        }
        return name;
    }
    if keysym & 0xff00_0000 == 0x0100_0000 {
        return format!("U{:04X}", keysym & 0x00ff_ffff);
    }
    format!("0x{keysym:08x}")
}

/// The chord of a key pressed with modifiers held.
///
/// # Arguments
/// * `modifiers` - The modifiers held.
/// * `keysym` - The key's unshifted keysym.
pub fn spec(modifiers: Modifiers, keysym: u32) -> String {
    let mut spec = String::new();
    for (held, prefix) in [
        (modifiers.ctrl, "C-"),
        (modifiers.alt, "A-"),
        (modifiers.shift, "S-"),
        (modifiers.logo, "W-"),
    ] {
        if held {
            spec.push_str(prefix);
        }
    }
    spec.push_str(&key_name(keysym));
    spec
}

/// A chord's modifiers as `C`, `A`, `S`, and `W` flags, and its key, the
/// prefixes openbox takes besides the four letters understood; `None` for
/// an empty chord or an unknown prefix.
fn split(spec: &str) -> Option<([bool; 4], String)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let mut parts: Vec<&str> = spec.split('-').collect();
    let mut key = parts.pop()?.to_string();
    if key.is_empty() && parts.len() >= 2 {
        parts.pop();
        key = "minus".to_string();
    }
    let mut held = [false; 4];
    for part in parts {
        let at = match part {
            "C" | "Control" | "Ctrl" => 0,
            "A" | "Alt" | "Mod1" => 1,
            "S" | "Shift" => 2,
            "W" | "Super" | "Mod4" | "M" | "Meta" | "H" | "Hyper" => 3,
            _ => return None,
        };
        held[at] = true;
    }
    if key.chars().count() == 1 {
        key = key.to_lowercase();
    }
    Some((held, key))
}

/// A chord in one spelling, so two that name the same keys compare equal:
/// the modifiers in the order `C`, `A`, `S`, `W`, a letter key in lower
/// case.  An empty or unreadable chord is left as it is.
///
/// # Arguments
/// * `spec` - The chord.
pub fn canonical(spec: &str) -> String {
    let Some((held, key)) = split(spec) else {
        return spec.trim().to_string();
    };
    let mut out = String::new();
    for (on, prefix) in held.iter().zip(["C-", "A-", "S-", "W-"]) {
        if *on {
            out.push_str(prefix);
        }
    }
    out.push_str(&key);
    out
}

/// A chord as a user reads it, the way the window manager labels the binds
/// it publishes: `Ctrl`, `Alt`, `Shift`, and `Super`, then the key, a letter
/// in upper case and the usual names for the others, joined by `+`.
///
/// # Arguments
/// * `spec` - The chord.
pub fn label(spec: &str) -> String {
    let Some((held, key)) = split(spec) else {
        return spec.to_string();
    };
    let mut parts: Vec<String> = held
        .iter()
        .zip(["Ctrl", "Alt", "Shift", "Super"])
        .filter(|(on, _)| **on)
        .map(|(_, name)| name.to_string())
        .collect();
    let key = match key.as_str() {
        "Super_L" | "Super_R" => "Super".to_string(),
        "Alt_L" | "Alt_R" => "Alt".to_string(),
        "Control_L" | "Control_R" => "Ctrl".to_string(),
        "Shift_L" | "Shift_R" => "Shift".to_string(),
        "Return" | "KP_Enter" => "Enter".to_string(),
        "space" => "Space".to_string(),
        "Escape" => "Esc".to_string(),
        "Prior" => "Page Up".to_string(),
        "Next" => "Page Down".to_string(),
        "minus" => "-".to_string(),
        "plus" => "+".to_string(),
        other if other.chars().count() == 1 => other.to_uppercase(),
        other => other.replace('_', " "),
    };
    parts.push(key);
    parts.join("+")
}

/// The keys of a chord's label, one per cap: split at `+`, a `+` key kept.
///
/// # Arguments
/// * `label` - The label, such as `Ctrl+Alt+T` or `Ctrl++`.
pub fn label_keys(label: &str) -> Vec<String> {
    let mut keys: Vec<String> = label
        .split('+')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .collect();
    if label.trim_end().ends_with('+') {
        keys.push("+".to_string());
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_published_file_parses_binds_and_command_binds() {
        let text = "W-Left\tW-Left\tSuper+Left\tTile the window to the left half\n\
                    C-W-S-Right\tC-W-S-Right\tCtrl+Shift+Super+Right\tMove the window to the next desktop\n\
                    \tA-F4\t\tClose the window\n\
                    W-t\tC-A-t\tSuper+T\tTerminal\n\
                    execute\tW-e\tSuper+E\tfoot -e htop\n\
                    Super+Left\tan old two-column line\n\
                    execute\t\t\tno chord\n\
                    \n";
        let published = parse(text);
        assert_eq!(published.binds.len(), 4);
        assert_eq!(published.binds[0].spec, "W-Left");
        assert!(!published.binds[0].changed());
        assert_eq!(published.binds[2].spec, "");
        assert_eq!(published.binds[2].origin, "A-F4");
        assert!(published.binds[2].changed());
        assert_eq!(published.binds[3].description, "Terminal");
        assert!(published.binds[3].changed());
        assert_eq!(
            published.custom,
            vec![Custom {
                spec: "W-e".to_string(),
                label: "Super+E".to_string(),
                command: "foot -e htop".to_string(),
            }]
        );
        assert_eq!(parse(""), Published::default());
    }

    #[test]
    fn a_rebind_merges_into_the_overrides_keeping_every_other_line() {
        let text =
            "# my keys\nrebind\tC-A-t\tW-t\nsomething new\tthe shell does not know\n\nexecute\tW-e\tfoot\n";
        let moved = with_rebind(text, "C-A-t", Some("W-Return"));
        assert_eq!(
            moved,
            "# my keys\nrebind\tC-A-t\tW-Return\nsomething new\tthe shell does not know\n\nexecute\tW-e\tfoot\n"
        );
        let added = with_rebind(text, "W-Left", Some("C-W-Left"));
        assert!(added.starts_with(text));
        assert!(added.ends_with("rebind\tW-Left\tC-W-Left\n"));
        let disabled = with_rebind("", "A-F4", Some(""));
        assert_eq!(disabled, "rebind\tA-F4\t\n");
        assert_eq!(
            parse_rebinds(&disabled),
            vec![("A-F4".to_string(), String::new())]
        );
        let doubled = "rebind\tC-A-t\tW-t\nrebind\tA-C-t\tW-y\n";
        assert_eq!(with_rebind(doubled, "C-A-t", Some("W-u")), "rebind\tC-A-t\tW-u\n");
    }

    /// The `rebind` lines of an overrides text, as origin and chord.
    fn parse_rebinds(text: &str) -> Vec<(String, String)> {
        text.lines()
            .filter_map(fields)
            .filter(|f| f.first() == Some(&"rebind"))
            .map(|f| (f[1].to_string(), f.get(2).unwrap_or(&"").to_string()))
            .collect()
    }

    #[test]
    fn a_reset_or_a_chord_back_at_its_origin_removes_the_rebind() {
        let text = "# keep\nrebind\tC-A-t\tW-t\nrebind\tW-Left\tC-Left\n";
        assert_eq!(
            with_rebind(text, "C-A-t", None),
            "# keep\nrebind\tW-Left\tC-Left\n"
        );
        assert_eq!(
            with_rebind(text, "W-Left", Some("W-Left")),
            "# keep\nrebind\tC-A-t\tW-t\n"
        );
        assert_eq!(with_rebind("rebind\tW-a\tW-b\n", "W-a", None), "");
        assert_eq!(with_rebind("", "W-a", None), "");
    }

    #[test]
    fn command_binds_are_added_once_and_removed_alone() {
        let text = "# keep\nrebind\tW-Left\tC-Left\n";
        let added = with_execute(text, "W-e", " foot -e htop ");
        assert_eq!(
            added,
            "# keep\nrebind\tW-Left\tC-Left\nexecute\tW-e\tfoot -e htop\n"
        );
        assert_eq!(with_execute(&added, "W-E", "foot -e htop"), added);
        let two = with_execute(&added, "W-e", "xterm");
        assert_eq!(
            without_execute(&two, "W-e", "foot -e htop"),
            "# keep\nrebind\tW-Left\tC-Left\nexecute\tW-e\txterm\n"
        );
        assert_eq!(without_execute(&added, "W-e", "xterm"), added);
        assert_eq!(without_execute("execute\tW-e\tfoot\n", "W-e", "foot"), "");
    }

    #[test]
    fn a_chord_is_spelled_from_the_modifiers_and_the_unshifted_keysym() {
        let none = Modifiers::default();
        let all = Modifiers {
            shift: true,
            ctrl: true,
            alt: true,
            logo: true,
        };
        let logo = Modifiers { logo: true, ..none };
        assert_eq!(spec(logo, Keysym::Left.raw()), "W-Left");
        assert_eq!(spec(all, Keysym::Right.raw()), "C-A-S-W-Right");
        let ctrl_alt = Modifiers {
            ctrl: true,
            alt: true,
            ..none
        };
        assert_eq!(spec(ctrl_alt, Keysym::t.raw()), "C-A-t");
        assert_eq!(spec(ctrl_alt, Keysym::T.raw()), "C-A-t");
        assert_eq!(spec(none, Keysym::Super_L.raw()), "Super_L");
        assert_eq!(spec(none, Keysym::F4.raw()), "F4");
        assert_eq!(spec(logo, Keysym::XF86_AudioMute.raw()), "W-XF86AudioMute");
        assert_eq!(spec(logo, Keysym::EuroSign.raw()), "W-EuroSign");
        assert_eq!(spec(logo, 0x0100_1234), "W-U1234");
        assert!(is_modifier(Keysym::Super_L.raw()));
        assert!(is_modifier(Keysym::Control_R.raw()));
        assert!(is_modifier(Keysym::ISO_Level3_Shift.raw()));
        assert!(!is_modifier(Keysym::Left.raw()));
    }

    #[test]
    fn chords_compare_in_one_spelling_and_read_as_labels() {
        assert_eq!(canonical("C-W-S-Right"), "C-S-W-Right");
        assert_eq!(canonical("W-A"), canonical("W-a"));
        assert_eq!(canonical("Mod4-Control-t"), "C-W-t");
        assert_eq!(canonical("C--"), "C-minus");
        assert_eq!(canonical(""), "");
        assert_eq!(label("C-W-S-Right"), "Ctrl+Shift+Super+Right");
        assert_eq!(label("C-A-t"), "Ctrl+Alt+T");
        assert_eq!(label("Super_L"), "Super");
        assert_eq!(label("W-Return"), "Super+Enter");
        assert_eq!(label("A-Page_Up"), "Alt+Page Up");
        assert_eq!(label_keys("Ctrl+Alt+T"), vec!["Ctrl", "Alt", "T"]);
        assert_eq!(label_keys("Ctrl++"), vec!["Ctrl", "+"]);
    }

    #[test]
    fn the_rows_sharing_a_chord_are_named_to_each_other() {
        let published = parse(
            "W-Left\tW-Left\tSuper+Left\tTile left\n\
             W-Left\tW-Up\tSuper+Left\tTile up\n\
             \tA-F4\t\tClose\n\
             execute\tW-Left\tSuper+Left\tfoot\n",
        );
        assert_eq!(
            published.others_on("W-Left", "Tile left"),
            vec!["Tile up", "foot"]
        );
        assert_eq!(
            published.others_on("W-Left", "foot"),
            vec!["Tile left", "Tile up"]
        );
        assert!(published.others_on("", "Close").is_empty());
        assert!(published.others_on("A-F4", "Close").is_empty());
    }
}
