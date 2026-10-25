//! Files the caller attaches (a file part, a PDF from Read), which arrive as
//! named data URLs and are saved under `$OMNI2API_HOME/files/<conv>/`.
//! Nothing is copied to or from the caller's disk: the seat works on the
//! caller's files in place with the caller's own tools.

use std::path::{Path, PathBuf};

use crate::{data_url_bytes, omni2api_home};

/// A data URL's `name=` parameter, decoded.
pub fn data_url_name(url: &str) -> Option<String> {
    let meta = url.strip_prefix("data:")?.split(',').next()?;
    let raw = meta.split(';').find_map(|p| p.strip_prefix("name="))?;
    let b = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).ok().filter(|n| !n.trim().is_empty())
}

/// A file name safe to create: no directories, no leading dots, no
/// characters Windows refuses.
pub fn safe_name(n: &str) -> String {
    let base = n.rsplit(['/', '\\']).next().unwrap_or(n);
    let mut o: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    o = o.trim().trim_start_matches('.').to_string();
    if o.is_empty() {
        o = "file".into();
    }
    o
}

fn ext_for(mime: &str) -> &str {
    match mime {
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "application/json" => "json",
        "application/zip" => "zip",
        m => m.rsplit('/').next().unwrap_or("bin"),
    }
}

pub fn conv_dir(conv: &str) -> PathBuf {
    omni2api_home()
        .join("files")
        .join(safe_name(conv.trim_start_matches("cnv_")))
}

/// Write `bytes` as `name` in `dir`; an identical file already there is
/// reused, a different one gets `-2`, `-3`… before the extension.
pub fn write_unique(dir: &Path, name: &str, bytes: &[u8]) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let name = safe_name(name);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((a, b)) if !a.is_empty() => (a.to_string(), format!(".{b}")),
        _ => (name.clone(), String::new()),
    };
    for i in 1..1000 {
        let p = if i == 1 {
            dir.join(&name)
        } else {
            dir.join(format!("{stem}-{i}{ext}"))
        };
        match std::fs::read(&p) {
            Ok(old) if old == bytes => return Some(p),
            Ok(_) => continue,
            Err(_) => return std::fs::write(&p, bytes).ok().map(|_| p),
        }
    }
    None
}

pub struct Saved {
    pub path: String,
    pub mime: String,
    pub size: usize,
    pub image: bool,
}

/// Save one attachment URL: pictures under images/ (as before), other files
/// under files/<conv>/ by their own name. A link is kept as a link.
pub fn save_url(url: &str, conv: &str, stem: &str, i: usize) -> Option<Saved> {
    let Some((mime, bytes)) = data_url_bytes(url) else {
        return (!url.is_empty()).then(|| Saved {
            path: url.to_string(),
            mime: String::new(),
            size: 0,
            image: !url.to_lowercase().ends_with(".pdf"),
        });
    };
    let image = mime.starts_with("image/");
    let path = if image {
        let dir = omni2api_home().join("images");
        let _ = std::fs::create_dir_all(&dir);
        let ext = ext_for(&mime).replace("jpeg", "jpg");
        let f = dir.join(format!("{stem}-{i}.{ext}"));
        std::fs::write(&f, &bytes).ok()?;
        f
    } else {
        let name = data_url_name(url).unwrap_or_else(|| format!("{stem}-{i}.{}", ext_for(&mime)));
        write_unique(&conv_dir(conv), &name, &bytes)?
    };
    Some(Saved {
        path: path.display().to_string(),
        mime,
        size: bytes.len(),
        image,
    })
}

pub fn describe(f: &Saved) -> String {
    if f.size == 0 {
        return f.path.clone();
    }
    format!("{}  ({}, {})", f.path, f.mime, human(f.size as u64))
}

pub fn human(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} bytes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rand_hex;

    #[test]
    fn names() {
        assert_eq!(
            data_url_name("data:application/pdf;name=a+b%2F.pdf;base64,x").as_deref(),
            Some("a b/.pdf")
        );
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("..hidden"), "hidden");
        assert_eq!(safe_name("a:b?.txt"), "a_b_.txt");
    }

    #[test]
    fn unique_names() {
        let d = std::env::temp_dir().join(format!("ohs-files-{}", rand_hex(6)));
        let a = write_unique(&d, "r.txt", b"1").unwrap();
        assert_eq!(write_unique(&d, "r.txt", b"1").unwrap(), a);
        let b = write_unique(&d, "r.txt", b"2").unwrap();
        assert!(b.ends_with("r-2.txt"));
        let _ = std::fs::remove_dir_all(d);
    }
}
