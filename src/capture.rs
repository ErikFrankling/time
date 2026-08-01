use anyhow::{bail, Context, Result};
use image::imageops::FilterType;
use image::DynamicImage;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct Window {
    pub class: String,
    pub title: String,
}

impl Window {
    pub fn describe(&self) -> String {
        match (self.class.is_empty(), self.title.is_empty()) {
            (true, true) => "unknown".into(),
            (false, true) => self.class.clone(),
            (true, false) => self.title.clone(),
            (false, false) => format!("{} — {}", self.class, self.title),
        }
    }

    /// Case-insensitive substring match against class and title.
    pub fn blocked(&self, blocklist: &[String]) -> bool {
        let hay = format!("{} {}", self.class, self.title).to_lowercase();
        blocklist
            .iter()
            .any(|b| !b.is_empty() && hay.contains(&b.to_lowercase()))
    }
}

fn hyprctl(args: &[&str]) -> Result<serde_json::Value> {
    let out = Command::new("hyprctl")
        .args(args)
        .arg("-j")
        .output()
        .context("running hyprctl (is Hyprland running?)")?;
    if !out.status.success() {
        bail!(
            "hyprctl {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

pub fn active_window() -> Window {
    // A missing/failed active window is normal (empty workspace), not an error.
    let v = match hyprctl(&["activewindow"]) {
        Ok(v) => v,
        Err(_) => return Window::default(),
    };
    Window {
        class: v["class"].as_str().unwrap_or("").to_string(),
        title: v["title"].as_str().unwrap_or("").to_string(),
    }
}

/// Name of the monitor that currently has focus, so multi-monitor setups
/// capture the screen actually being looked at rather than all of them.
pub fn focused_monitor() -> Option<String> {
    let v = hyprctl(&["monitors"]).ok()?;
    let arr = v.as_array()?;
    arr.iter()
        .find(|m| m["focused"].as_bool().unwrap_or(false))
        .or_else(|| arr.first())
        .and_then(|m| m["name"].as_str())
        .map(|s| s.to_string())
}

/// Screenshot the focused monitor and downscale it. The PNG from grim is piped
/// through stdout and decoded in-memory -- nothing is ever written to disk.
pub fn screenshot(width: u32) -> Result<DynamicImage> {
    let mut cmd = Command::new("grim");
    if let Some(mon) = focused_monitor() {
        cmd.arg("-o").arg(mon);
    }
    // "-" means stdout.
    let out = cmd.arg("-").output().context("running grim")?;
    if !out.status.success() {
        bail!("grim failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    let img = image::load_from_memory(&out.stdout).context("decoding grim output")?;
    Ok(if img.width() > width {
        let height = img.height() * width / img.width().max(1);
        img.resize_exact(width, height, FilterType::Triangle)
    } else {
        img
    })
}

pub fn to_jpeg(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut buf = std::io::Cursor::new(Vec::new());
    img.to_rgb8()
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut buf, 72,
        ))?;
    Ok(buf.into_inner())
}

/// Difference hash. Downscale to 9x8 grayscale and record whether each pixel is
/// brighter than its right-hand neighbour -- 64 comparisons, 64 bits.
///
/// This is the whole idle detector: if consecutive minutes hash the same, the
/// screen didn't change, and there's no reason to spend an API call on it.
pub fn dhash(img: &DynamicImage) -> u64 {
    let small = img.resize_exact(9, 8, FilterType::Triangle).to_luma8();
    let mut hash = 0u64;
    let mut bit = 0;
    for y in 0..8 {
        for x in 0..8 {
            if small.get_pixel(x, y)[0] > small.get_pixel(x + 1, y)[0] {
                hash |= 1 << bit;
            }
            bit += 1;
        }
    }
    hash
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}
