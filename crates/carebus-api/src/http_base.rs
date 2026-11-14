//! Parsed base URLs for this service's own HTTP clients.
//!
//! Every admin client here (Keycloak, licences, the service-account token endpoint, the MCP server) talks to a
//! configured base and appends a path. Built with `format!("{base}/{path}")`, neither the scheme/host nor the path
//! segments are constrained: a segment that ever carries caller data can add a path of its own (`../`, an absolute
//! path, a `?query`). [`BaseUrl`] parses the base once, restricting it to `http`/`https` with no query or fragment,
//! and builds each request URL through [`Url::path_segments_mut`], which percent-encodes every segment
//! (`/` → `%2F`), so a segment can never leave the base path.
use reqwest::Url;

/// Why a configured base URL cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BaseUrlError {
    #[error("`{0}` is not an absolute URL with a host")]
    NotAbsolute(String),
    #[error("`{0}` must be an http(s) URL")]
    Scheme(String),
    #[error("`{0}` must not carry a query or a fragment")]
    QueryOrFragment(String),
}

/// A base URL parsed and checked once: `http`/`https`, a host, no query or fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseUrl(Url);

impl BaseUrl {
    /// Parse `raw`, keeping the old trailing-slash normalisation (`trim_end_matches('/')`): the base is a directory
    /// that [`Self::endpoint`] appends to. Anything that is not an absolute `http`/`https` URL, or that carries a
    /// query or a fragment, is refused — a base is a location, not a request.
    pub fn parse(raw: &str) -> Result<Self, BaseUrlError> {
        let mut url = Url::parse(raw).map_err(|_| BaseUrlError::NotAbsolute(raw.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(BaseUrlError::Scheme(raw.to_string()));
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err(BaseUrlError::NotAbsolute(raw.to_string()));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(BaseUrlError::QueryOrFragment(raw.to_string()));
        }
        if url.path() != "/" {
            let trimmed = url.path().trim_end_matches('/').to_string();
            url.set_path(&trimmed);
        }
        Ok(Self(url))
    }

    /// `segments` appended to the base path, each percent-encoded as ONE path segment (`/` → `%2F`). A segment that
    /// is exactly `.` or `..` is dropped by the `url` crate rather than encoded; either way the request cannot leave
    /// the base path.
    pub fn endpoint(&self, segments: &[&str]) -> Url {
        self.endpoint_with_query(segments, &[])
    }

    /// [`Self::endpoint`], plus `query` as the URL's query string (pairs percent-encoded, order kept).
    pub fn endpoint_with_query(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = self.0.clone();
        {
            let mut path = url
                .path_segments_mut()
                .expect("an http(s) URL with a host is always a base");
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        url
    }

    /// The base URL as parsed. A bare host keeps the URL's canonical trailing `/` (`http://mcp.test/`); every
    /// other base has its trailing slashes trimmed, as the clients did before (`trim_end_matches('/')`).
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The base URL itself (no segment appended).
    pub fn as_url(&self) -> &Url {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_segments_under_the_base() {
        let base = BaseUrl::parse("https://kc.test/admin/realms/portal/").unwrap();
        assert_eq!(base.as_str(), "https://kc.test/admin/realms/portal");
        assert_eq!(
            base.endpoint(&["users", "5f2a"]).as_str(),
            "https://kc.test/admin/realms/portal/users/5f2a"
        );
        assert_eq!(
            base.endpoint(&[]).as_str(),
            "https://kc.test/admin/realms/portal"
        );
        let root = BaseUrl::parse("http://mcp.test").unwrap();
        assert_eq!(
            root.endpoint(&["mcp", "internal"]).as_str(),
            "http://mcp.test/mcp/internal"
        );
    }

    #[test]
    fn a_hostile_segment_is_encoded_and_stays_under_the_base() {
        let base = BaseUrl::parse("https://lic.test/admin").unwrap();
        let url = base.endpoint(&["licenses", "../../tenants", "x?y=1"]);
        assert_eq!(
            url.as_str(),
            "https://lic.test/admin/licenses/..%2F..%2Ftenants/x%3Fy=1"
        );
        assert!(url.as_str().starts_with("https://lic.test/admin/"), "{url}");
        // A segment that is exactly `..` cannot traverse either: the url crate drops it.
        assert_eq!(
            base.endpoint(&["..", "users"]).as_str(),
            "https://lic.test/admin/users"
        );
    }

    #[test]
    fn only_http_and_https_with_a_host_are_accepted() {
        assert!(matches!(
            BaseUrl::parse("ftp://files.test/pub"),
            Err(BaseUrlError::Scheme(_))
        ));
        assert!(BaseUrl::parse("file:///etc/passwd").is_err());
        assert!(BaseUrl::parse("not a url").is_err());
        assert!(BaseUrl::parse("/relative/path").is_err());
        assert!(matches!(
            BaseUrl::parse("http://"),
            Err(BaseUrlError::NotAbsolute(_))
        ));
    }

    #[test]
    fn a_base_carrying_a_query_or_fragment_is_rejected() {
        assert!(matches!(
            BaseUrl::parse("https://lic.test/admin?tenant=1"),
            Err(BaseUrlError::QueryOrFragment(_))
        ));
        assert!(matches!(
            BaseUrl::parse("https://lic.test/admin#x"),
            Err(BaseUrlError::QueryOrFragment(_))
        ));
    }

    #[test]
    fn a_query_joins_with_encoded_pairs() {
        let base = BaseUrl::parse("https://kc.test/admin/realms/portal").unwrap();
        assert_eq!(
            base.endpoint_with_query(
                &["users"],
                &[("email", "ann@globex.com"), ("exact", "true"), ("max", "2")]
            )
            .as_str(),
            "https://kc.test/admin/realms/portal/users?email=ann%40globex.com&exact=true&max=2"
        );
    }
}
