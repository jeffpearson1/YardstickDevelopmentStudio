//! Lightweight syntax colouring for recipe YAML and the PowerShell inside it.

use std::sync::{Arc, LazyLock};

use eframe::egui::cache::{ComputerMut, FrameCache};
use eframe::egui::{Color32, FontId, Galley, TextFormat, Ui, text::LayoutJob};
use regex::Regex;

struct Palette {
    text: Color32,
    key: Color32,
    comment: Color32,
    variable: Color32,
    placeholder: Color32,
    string: Color32,
    keyword: Color32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            text: Color32::from_rgb(212, 212, 212),
            key: Color32::from_rgb(110, 170, 255),
            comment: Color32::from_rgb(110, 150, 100),
            variable: Color32::from_rgb(120, 210, 230),
            placeholder: Color32::from_rgb(240, 170, 80),
            string: Color32::from_rgb(206, 145, 120),
            keyword: Color32::from_rgb(200, 140, 220),
        }
    } else {
        Palette {
            text: Color32::from_rgb(30, 30, 30),
            key: Color32::from_rgb(0, 80, 180),
            comment: Color32::from_rgb(60, 120, 50),
            variable: Color32::from_rgb(0, 120, 140),
            placeholder: Color32::from_rgb(190, 100, 0),
            string: Color32::from_rgb(160, 60, 30),
            keyword: Color32::from_rgb(140, 40, 160),
        }
    }
}

static KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Za-z0-9_]+)(\s*:)").unwrap());
static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?P<comment>(?:^|\s)#.*$)|(?P<var>\$[A-Za-z_][\w:]*|\$\{[^}]*\})|(?P<ph><(?:filename|version|productcode)>)|(?P<str>'[^'\n]*'|"[^"\n]*")|(?P<kw>\b(?:if|else|elseif|foreach|for|return|function|param|try|catch|finally|throw|true|false|null|exit)\b)"#,
    )
    .unwrap()
});

/// Colours recipe YAML: top-level keys, comments, `$variables`, `<placeholders>`, strings.
pub fn yaml_job(text: &str, font_size: f32, dark: bool) -> LayoutJob {
    let p = palette(dark);
    let font = FontId::monospace(font_size);
    let fmt = |c: Color32| TextFormat::simple(font.clone(), c);
    let mut job = LayoutJob::default();
    for line in text.split_inclusive('\n') {
        let mut rest = line;
        if let Some(c) = KEY.captures(line) {
            let key_end = c.get(1).unwrap().end();
            let colon_end = c.get(2).unwrap().end();
            job.append(&line[..key_end], 0.0, fmt(p.key));
            job.append(&line[key_end..colon_end], 0.0, fmt(p.text));
            rest = &line[colon_end..];
        }
        let mut last = 0;
        for m in TOKENS.captures_iter(rest) {
            let (whole, color) = if let Some(x) = m.name("comment") {
                (x, p.comment)
            } else if let Some(x) = m.name("var") {
                (x, p.variable)
            } else if let Some(x) = m.name("ph") {
                (x, p.placeholder)
            } else if let Some(x) = m.name("str") {
                (x, p.string)
            } else if let Some(x) = m.name("kw") {
                (x, p.keyword)
            } else {
                continue;
            };
            if whole.start() > last {
                job.append(&rest[last..whole.start()], 0.0, fmt(p.text));
            }
            job.append(whole.as_str(), 0.0, fmt(color));
            last = whole.end();
        }
        if last < rest.len() {
            job.append(&rest[last..], 0.0, fmt(p.text));
        }
    }
    job
}

/// Lays out highlighted YAML for a `TextEdit` layouter. The highlighting is
/// memoised per (text, size, theme), so unchanged editors cost nothing extra
/// per frame; egui caches the galley itself.
pub fn yaml_galley(ui: &Ui, text: &str, font_size: f32, dark: bool, wrap: f32) -> Arc<Galley> {
    #[derive(Default)]
    struct Highlighter;
    impl<'a> ComputerMut<(&'a str, u32, bool), LayoutJob> for Highlighter {
        fn compute(&mut self, (text, size, dark): (&'a str, u32, bool)) -> LayoutJob {
            yaml_job(text, f32::from_bits(size), dark)
        }
    }
    let mut job = ui.ctx().memory_mut(|m| {
        m.caches.cache::<FrameCache<LayoutJob, Highlighter>>().get((text, font_size.to_bits(), dark)).clone()
    });
    job.wrap.max_width = wrap;
    ui.ctx().fonts_mut(|f| f.layout_job(job))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_text_exactly() {
        let src = "id: app\npreDownloadScript: |\n  $version = '1.0' # note\n  <filename>\n\n# end";
        let job = yaml_job(src, 13.0, true);
        assert_eq!(job.text, src);
    }
}
