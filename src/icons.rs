//! Tray icons as symbolic SVGs, installed into the user's hicolor theme so the tray host can tint them with
//! its theme colour (GTK hosts recolour `-symbolic` icons). GTK recolours fills only, so every shape is a
//! filled path; low/charging battery use the `error`/`success` classes to keep red/green.

/// At or below this charge (and not charging) the battery fill turns red.
const LOW_BATTERY: i64 = 30;

use crate::ipc::Device;
use std::path::PathBuf;

/// `M.. z` path for a rounded rectangle.
fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> String {
    let (iw, ih) = (w - 2.0 * r, h - 2.0 * r);
    format!(
        "M{:.2} {y:.2}h{iw:.2}a{r} {r} 0 0 1 {r} {r}v{ih:.2}a{r} {r} 0 0 1 -{r} {r}h-{iw:.2}a{r} {r} 0 0 1 -{r} -{r}v-{ih:.2}a{r} {r} 0 0 1 {r} -{r}z",
        x + r
    )
}

/// A thick arc (annulus sector with round caps) centred on (cx, cy), spanning ±`deg` around the +x axis.
fn wave(cx: f32, cy: f32, radius: f32, deg: f32, half_width: f32) -> String {
    let p = |r: f32, a: f32| (cx + r * a.to_radians().cos(), cy + r * a.to_radians().sin());
    let (ro, ri, w) = (radius + half_width, radius - half_width, half_width);
    let ((x0, y0), (x1, y1), (x2, y2), (x3, y3)) = (p(ro, -deg), p(ro, deg), p(ri, deg), p(ri, -deg));
    format!(
        "M{x0:.2} {y0:.2}A{ro} {ro} 0 0 1 {x1:.2} {y1:.2}A{w} {w} 0 0 1 {x2:.2} {y2:.2}A{ri} {ri} 0 0 0 {x3:.2} {y3:.2}A{w} {w} 0 0 1 {x0:.2} {y0:.2}z"
    )
}

fn circle(cx: f32, cy: f32, r: f32) -> String {
    format!("M{:.2} {cy}a{r} {r} 0 1 0 {d} 0a{r} {r} 0 1 0 -{d} 0z", cx - r, d = 2.0 * r)
}

enum Badge {
    Waves,
    Cross,
    Alert,
}

/// `fill`: battery level in tenths plus an optional GTK colour class (`error` = red, `success` = green).
fn svg(fill: Option<(u8, &str)>, badge: Badge, dim: bool) -> String {
    let body = format!("{}{}", rrect(1.5, 0.5, 14.2, 23.0, 4.1), rrect(3.7, 2.7, 9.8, 18.6, 1.9));
    let opacity = if dim { r#" opacity="0.45""# } else { "" };
    let mut out = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill-rule="evenodd" d="{body}"{opacity}/>"#);
    if let Some((level, class)) = fill {
        let h = (15.6 * f32::from(level) / 10.0).max(1.4); // screen spans y 4.2..19.8
        out += &format!(r#"<path class="{class}" d="{}"/>"#, rrect(5.3, 19.8 - h, 6.8, h, 1.2_f32.min(h / 2.0)));
    }
    out += &match badge {
        Badge::Waves => format!(r#"<path d="{}{}"/>"#, wave(14.5, 12.0, 4.6, 47.6, 1.1), wave(14.64, 12.0, 8.6, 46.1, 1.1)),
        Badge::Cross => [45, -45]
            .map(|a| format!(r#"<rect x="15.55" y="10.6" width="8.7" height="2.2" rx="1.1" transform="rotate({a} 19.9 11.7)"/>"#))
            .concat(),
        Badge::Alert => format!(
            r#"<path class="warning" fill-rule="evenodd" d="{}{}{}"/>"#,
            circle(19.2, 12.0, 4.4),
            rrect(18.25, 8.65, 1.9, 4.7, 0.95),
            circle(19.2, 14.6, 1.05)
        ),
    };
    out + "</svg>"
}

/// Every icon this indicator can show, by theme name.
fn all() -> Vec<(String, String)> {
    let mut v = Vec::new();
    for l in 0..=10u8 {
        v.push((format!("kcd-indicator-battery-{l}-symbolic"), svg(Some((l, "")), Badge::Waves, false)));
        v.push((format!("kcd-indicator-charging-{l}-symbolic"), svg(Some((l, "success")), Badge::Waves, false)));
    }
    // Red variants: <= LOW_BATTERY rounds to at most level 3.
    for l in 0..=3u8 {
        v.push((format!("kcd-indicator-battery-low-{l}-symbolic"), svg(Some((l, "error")), Badge::Waves, false)));
    }
    v.push(("kcd-indicator-connected-symbolic".into(), svg(None, Badge::Waves, false)));
    v.push(("kcd-indicator-disconnected-symbolic".into(), svg(None, Badge::Cross, true)));
    v.push(("kcd-indicator-pair-symbolic".into(), svg(None, Badge::Alert, false)));
    v
}

/// Write the icons to `$XDG_DATA_HOME/icons/hicolor/scalable/status`, skipping unchanged files.
pub fn install() -> std::io::Result<()> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or_else(|| std::io::Error::other("neither XDG_DATA_HOME nor HOME is set"))?;
    let theme = data.join("icons/hicolor");
    let dir = theme.join("scalable/status");
    std::fs::create_dir_all(&dir)?;
    let mut changed = false;
    for (name, svg) in all() {
        let path = dir.join(format!("{name}.svg"));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg.as_str()) {
            std::fs::write(path, svg)?;
            changed = true;
        }
    }
    // GTK only rescans a theme when the theme dir's mtime changes, not its subdirs': bump it like icon
    // package hooks do, or running hosts never see newly added icons.
    if changed {
        std::fs::File::open(&theme)?.set_modified(std::time::SystemTime::now())?;
    }
    Ok(())
}

/// Icon name for the current state: a pending pair request wins, then the first connected paired device.
pub fn name(devices: &[Device]) -> String {
    if devices.iter().any(|d| d.state == "PAIR_REQUESTED_BY_PEER") {
        return "kcd-indicator-pair-symbolic".into();
    }
    match devices.iter().find(|d| d.connected && d.state == "PAIRED") {
        None => "kcd-indicator-disconnected-symbolic".into(),
        Some(Device { battery: None, .. }) => "kcd-indicator-connected-symbolic".into(),
        Some(Device { battery: Some(b), .. }) => {
            let level = (b.charge.clamp(0, 100) + 5) / 10;
            let kind = match (b.charging, b.charge <= LOW_BATTERY) {
                (true, _) => "charging",
                (false, true) => "battery-low",
                (false, false) => "battery",
            };
            format!("kcd-indicator-{kind}-{level}-symbolic")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Battery;

    fn dev(state: &str, connected: bool, battery: Option<(i64, bool)>) -> Device {
        Device {
            state: state.into(),
            connected,
            battery: battery.map(|(charge, charging)| Battery { charge, charging }),
            ..Default::default()
        }
    }

    #[test]
    fn picks_icon_for_state() {
        let names: Vec<String> = all().into_iter().map(|(n, _)| n).collect();
        let cases = [
            (vec![], "kcd-indicator-disconnected-symbolic"),
            (vec![dev("PAIRED", false, Some((80, false)))], "kcd-indicator-disconnected-symbolic"),
            (vec![dev("UNPAIRED", true, None)], "kcd-indicator-disconnected-symbolic"),
            (vec![dev("PAIRED", true, None)], "kcd-indicator-connected-symbolic"),
            (vec![dev("PAIRED", true, Some((53, false)))], "kcd-indicator-battery-5-symbolic"),
            (vec![dev("PAIRED", true, Some((100, false)))], "kcd-indicator-battery-10-symbolic"),
            (vec![dev("PAIRED", true, Some((45, true)))], "kcd-indicator-charging-5-symbolic"),
            (vec![dev("PAIRED", true, Some((30, false)))], "kcd-indicator-battery-low-3-symbolic"),
            (vec![dev("PAIRED", true, Some((31, false)))], "kcd-indicator-battery-3-symbolic"),
            (vec![dev("PAIRED", true, Some((0, false)))], "kcd-indicator-battery-low-0-symbolic"),
            (vec![dev("PAIRED", true, Some((20, true)))], "kcd-indicator-charging-2-symbolic"),
            (vec![dev("PAIRED", true, Some((53, false))), dev("PAIR_REQUESTED_BY_PEER", true, None)], "kcd-indicator-pair-symbolic"),
        ];
        for (devices, want) in cases {
            assert_eq!(name(&devices), want);
            assert!(names.contains(&want.to_string()), "{want} is not generated");
        }
    }
}
