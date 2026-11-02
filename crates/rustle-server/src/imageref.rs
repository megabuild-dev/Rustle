//! Strict image reference parser. The repo match used by the webhook depends on it, so it
//! normalizes (host case, default tag) and rejects anything ambiguous instead of guessing.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub host: String,
    /// Lowercase repository path, e.g. `owner/app`.
    pub path: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid image reference: {0}")]
pub struct ImageRefError(pub &'static str);

fn valid_component(c: &str) -> bool {
    // [a-z0-9]+([._-]|__|-+[a-z0-9]+)* simplified: alnum runs separated by single . _ or -
    let b = c.as_bytes();
    if b.is_empty() || b.len() > 128 {
        return false;
    }
    let alnum = |x: u8| x.is_ascii_lowercase() || x.is_ascii_digit();
    if !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return false;
    }
    let mut prev_sep = false;
    for &x in b {
        if alnum(x) {
            prev_sep = false;
        } else if x == b'.' || x == b'_' || x == b'-' {
            if prev_sep && x != b'-' {
                return false;
            }
            prev_sep = true;
        } else {
            return false;
        }
    }
    true
}

fn valid_tag(t: &str) -> bool {
    let b = t.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-')
}

fn valid_digest(d: &str) -> bool {
    match d.strip_prefix("sha256:") {
        Some(h) => {
            h.len() == 64
                && h.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        }
        None => false,
    }
}

impl ImageRef {
    pub fn parse(input: &str) -> Result<ImageRef, ImageRefError> {
        if input.is_empty() || input.len() > 255 {
            return Err(ImageRefError("length"));
        }
        if input
            .bytes()
            .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
        {
            return Err(ImageRefError("whitespace"));
        }
        let (rest, digest) = match input.rsplit_once('@') {
            Some((r, d)) => {
                if !valid_digest(d) {
                    return Err(ImageRefError("digest"));
                }
                (r, Some(d.to_string()))
            }
            None => (input, None),
        };
        // A tag is the part after the last ':' that comes after the last '/'.
        let last_slash = rest.rfind('/').map(|i| i as isize).unwrap_or(-1);
        let (name, tag) = match rest.rfind(':') {
            Some(i) if (i as isize) > last_slash => {
                let t = &rest[i + 1..];
                if !valid_tag(t) {
                    return Err(ImageRefError("tag"));
                }
                (&rest[..i], Some(t.to_string()))
            }
            _ => (rest, None),
        };
        let mut parts: Vec<&str> = name.split('/').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(ImageRefError("empty path component"));
        }
        let first = parts[0];
        let (host, path_parts): (String, Vec<&str>) = if parts.len() > 1
            && (first.contains('.') || first.contains(':') || first == "localhost")
        {
            let h = first.to_ascii_lowercase();
            parts.remove(0);
            (h, parts)
        } else {
            let mut p = parts;
            if p.len() == 1 {
                p.insert(0, "library");
            }
            ("docker.io".to_string(), p)
        };
        if path_parts.is_empty() || !path_parts.iter().all(|c| valid_component(c)) {
            return Err(ImageRefError("repository path"));
        }
        if host.len() > 253
            || !host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b':')
        {
            return Err(ImageRefError("registry host"));
        }
        Ok(ImageRef {
            host,
            path: path_parts.join("/"),
            tag,
            digest,
        })
    }

    /// `host/path`, the identity compared against a site's registered repository.
    pub fn repo_key(&self) -> String {
        format!("{}/{}", self.host, self.path)
    }

    pub fn is_ghcr(&self) -> bool {
        self.host == "ghcr.io"
    }

    /// Tag to pull when no digest is given (`latest` default).
    pub fn pull_tag(&self) -> String {
        match (&self.digest, &self.tag) {
            (Some(d), _) => d.clone(),
            (None, Some(t)) => t.clone(),
            (None, None) => "latest".into(),
        }
    }

    pub fn with_digest(&self, digest: &str) -> String {
        format!("{}@{}", self.repo_key(), digest)
    }

    /// Canonical string: digest wins over tag, tag defaults to latest.
    pub fn canonical(&self) -> String {
        match (&self.digest, &self.tag) {
            (Some(d), _) => format!("{}@{}", self.repo_key(), d),
            (None, Some(t)) => format!("{}:{}", self.repo_key(), t),
            (None, None) => format!("{}:latest", self.repo_key()),
        }
    }
}

/// Validate a site's registered repository (no tag/digest, ghcr.io only in the MVP).
pub fn parse_repo(input: &str) -> Result<ImageRef, ImageRefError> {
    let r = ImageRef::parse(input)?;
    if r.tag.is_some() || r.digest.is_some() {
        return Err(ImageRefError("repository must not include a tag or digest"));
    }
    if !r.is_ghcr() {
        return Err(ImageRefError("only ghcr.io images are supported"));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn component_rules_exact_boundaries() {
        let ok = |s: &str| valid_component(s);
        assert!(ok("a"));
        assert!(ok(&"a".repeat(128)));
        assert!(!ok(&"a".repeat(129)));
        assert!(!ok(""));
        // separators allowed inside, never at the ends
        assert!(ok("my-repo") && ok("my.repo") && ok("my_repo") && ok("a0"));
        assert!(!ok("-a") && !ok("a-") && !ok(".a") && !ok("a.") && !ok("_a") && !ok("a_"));
        // repeated separators: only dashes may repeat
        assert!(ok("a--b"));
        assert!(!ok("a..b") && !ok("a__b") && !ok("a._b") && !ok("a-.b") && !ok("a--.b"));
        assert!(
            ok("a.-b") && ok("a_-b"),
            "a dash may follow another separator"
        );
        // uppercase and other characters are rejected
        assert!(!ok("Ab") && !ok("a b") && !ok("a/b") && !ok("a:b"));
    }

    #[test]
    fn tag_rules_exact_boundaries() {
        assert!(valid_tag("v1.2-3_x"));
        assert!(valid_tag("_x") && valid_tag("0"));
        assert!(valid_tag(&"a".repeat(128)));
        assert!(!valid_tag(&"a".repeat(129)));
        assert!(!valid_tag("") && !valid_tag("-x") && !valid_tag(".x"));
        assert!(!valid_tag("a b") && !valid_tag("a/b") && !valid_tag("a:b"));
    }

    #[test]
    fn parse_boundaries() {
        // overall length: 255 ok, 256 rejected
        let name = |n: usize| {
            format!(
                "{}/{}/{}",
                "x".repeat(100),
                "y".repeat(100),
                "z".repeat(n - 202)
            )
        };
        assert!(ImageRef::parse(&name(255)).is_ok());
        assert_eq!(ImageRef::parse(&name(256)).unwrap_err().0, "length");
        assert_eq!(ImageRef::parse("").unwrap_err().0, "length");
        // whitespace and control characters are each rejected on their own
        for bad in ["a b", "a\tb", "a\nb", "a\u{1}b", "a\rb"] {
            assert_eq!(ImageRef::parse(bad).unwrap_err().0, "whitespace", "{bad:?}");
        }
        // a registry host of exactly 253 chars is accepted; its character set is checked
        let h253 = format!("{}.{}", "h".repeat(126), "i".repeat(126));
        assert_eq!(h253.len(), 253);
        assert_eq!(ImageRef::parse(&format!("{h253}/a")).unwrap().host, h253);
        assert!(
            ImageRef::parse("ex_ample.com/a").is_err(),
            "underscore in host"
        );
        assert_eq!(ImageRef::parse("GHCR.io/o/a").unwrap().host, "ghcr.io");
        // what counts as a registry: dot, port or localhost in the first component
        assert_eq!(ImageRef::parse("localhost/a").unwrap().host, "localhost");
        assert_eq!(
            ImageRef::parse("localhost:5000/a").unwrap().host,
            "localhost:5000"
        );
        assert_eq!(ImageRef::parse("reg:5000/a").unwrap().host, "reg:5000");
        assert_eq!(ImageRef::parse("user/app").unwrap().host, "docker.io");
        assert_eq!(ImageRef::parse("user/app").unwrap().path, "user/app");
        assert_eq!(ImageRef::parse("app").unwrap().path, "library/app");
        // a single component that looks like a host is just a repo name
        assert_eq!(ImageRef::parse("a.b").unwrap().path, "library/a.b");
        // a registry with nothing after it
        assert!(ImageRef::parse("ghcr.io/").is_err());
        assert!(ImageRef::parse("ghcr.io//a").is_err());
        // digest and tag together
        let d = format!("sha256:{}", "a".repeat(64));
        let r = ImageRef::parse(&format!("ghcr.io/o/a:v1@{d}")).unwrap();
        assert_eq!(
            (r.tag.as_deref(), r.digest.as_deref()),
            (Some("v1"), Some(d.as_str()))
        );
        assert_eq!(
            ImageRef::parse("ghcr.io/o/a@sha256:xyz").unwrap_err().0,
            "digest"
        );
        assert_eq!(ImageRef::parse("ghcr.io/o/a:-bad").unwrap_err().0, "tag");
    }

    #[test]
    fn digest_and_untagged_name_edges() {
        let hex = "a".repeat(64);
        assert!(valid_digest(&format!("sha256:{hex}")));
        assert!(!valid_digest(&format!("sha256:{}", "a".repeat(63))));
        assert!(!valid_digest(&format!("sha256:{}", "a".repeat(65))));
        assert!(
            !valid_digest(&format!("sha256:{}g", "a".repeat(63))),
            "64 chars, one not hex"
        );
        assert!(
            !valid_digest(&format!("sha256:{}A", "a".repeat(63))),
            "uppercase is not hex here"
        );
        assert!(!valid_digest(&hex), "missing algorithm prefix");
        // no '/' at all: the ':' still introduces a tag
        let r = ImageRef::parse("app:v1").unwrap();
        assert_eq!(
            (r.path.as_str(), r.tag.as_deref()),
            ("library/app", Some("v1"))
        );
    }

    #[test]
    fn basic_forms() {
        let r = ImageRef::parse("ghcr.io/Owner/app").err();
        assert!(r.is_some(), "uppercase repo path is rejected");
        let r = ImageRef::parse("GHCR.IO/owner/app:v1").unwrap();
        assert_eq!(r.repo_key(), "ghcr.io/owner/app");
        assert_eq!(r.canonical(), "ghcr.io/owner/app:v1");
        let r = ImageRef::parse("ghcr.io/owner/app").unwrap();
        assert_eq!(r.canonical(), "ghcr.io/owner/app:latest");
        assert_eq!(r.pull_tag(), "latest");
        let r = ImageRef::parse(&format!("ghcr.io/owner/app:v1@{D}")).unwrap();
        assert_eq!(r.canonical(), format!("ghcr.io/owner/app@{D}"));
        assert_eq!(r.pull_tag(), D);
        assert_eq!(r.with_digest(D), format!("ghcr.io/owner/app@{D}"));
    }

    #[test]
    fn defaults_to_docker_hub() {
        let r = ImageRef::parse("nginx").unwrap();
        assert_eq!(r.repo_key(), "docker.io/library/nginx");
        assert!(!r.is_ghcr());
        let r = ImageRef::parse("user/app:1").unwrap();
        assert_eq!(r.repo_key(), "docker.io/user/app");
    }

    #[test]
    fn registry_port_is_not_a_tag() {
        let r = ImageRef::parse("localhost:5000/app").unwrap();
        assert_eq!(r.host, "localhost:5000");
        assert_eq!(r.tag, None);
        let r = ImageRef::parse("localhost:5000/app:2").unwrap();
        assert_eq!(r.tag.as_deref(), Some("2"));
    }

    #[test]
    fn rejects_bypass_attempts() {
        for bad in [
            "",
            " ghcr.io/o/a",
            "ghcr.io/o/a b",
            "ghcr.io//o/a",
            "ghcr.io/o/a:",
            "ghcr.io/o/a:../x",
            "ghcr.io/o/a:-x",
            "ghcr.io/o/a@sha256:xyz",
            "ghcr.io/o/a@md5:abc",
            "ghcr.io/o/../a",
            "ghcr.io/o/a/",
            "ghcr.io/o/a\n:1",
            "ghcr.io/o/-a",
            "ghcr.io/o/a..b",
            "ghcr.io/o/a;rm",
            "ghcr.io/o/a$x",
        ] {
            assert!(ImageRef::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn site_repo_rules() {
        assert!(parse_repo("ghcr.io/o/app").is_ok());
        assert!(parse_repo("ghcr.io/o/app:latest").is_err());
        assert!(parse_repo("docker.io/o/app").is_err());
        assert!(parse_repo("evil.example.com/o/app").is_err());
        // same repo, different spelling still matches by key
        assert_eq!(
            parse_repo("ghcr.io/o/app").unwrap().repo_key(),
            ImageRef::parse("GHCR.io/o/app:v2").unwrap().repo_key()
        );
        // lookalike hosts do not match
        assert_ne!(
            parse_repo("ghcr.io/o/app").unwrap().repo_key(),
            ImageRef::parse("ghcr.io.evil.com/o/app")
                .unwrap()
                .repo_key()
        );
    }
}
