//! A route table: which handler answers a request, by method and path. It is only the lookup, so
//! the bank's admin requests (bank/admin) and the front's use one format; each table fixes the
//! handler type it holds.
//!
//! A path ending in `*` is a prefix ("/api/plugins*"); the method "*" is any. The first route that
//! matches wins, so a prefix goes after the exact paths it must not take.

/// One line of a table.
pub struct Route<H> {
    pub method: &'static str,
    pub path: &'static str,
    pub handler: H,
}

/// A line of a table: `route("GET", "/x/status", status)`.
pub const fn route<H>(method: &'static str, path: &'static str, handler: H) -> Route<H> {
    Route { method, path, handler }
}

impl<H> Route<H> {
    pub fn matches(&self, method: &str, path: &str) -> bool {
        (self.method == "*" || self.method == method)
            && match self.path.strip_suffix('*') {
                Some(prefix) => path.starts_with(prefix),
                None => self.path == path,
            }
    }
}

/// The handler of the first route that matches.
pub fn find<'a, H>(routes: &'a [Route<H>], method: &str, path: &str) -> Option<&'a H> {
    routes.iter().find(|r| r.matches(method, path)).map(|r| &r.handler)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_matching_route_wins_prefixes_and_any_method_included() {
        let table = [route("GET", "/x/status", 1), route("POST", "/x/save", 2), route("*", "/x/plugins*", 3), route("POST", "/x/plugins/x", 4), route("GET", "/x/*", 5)];
        let at = |m: &str, p: &str| find(&table, m, p).copied();
        assert_eq!((at("GET", "/x/status"), at("POST", "/x/save"), at("GET", "/x/save")), (Some(1), Some(2), Some(5)));
        // A prefix takes its whole subtree, any method; routes after it never see those paths.
        assert_eq!((at("GET", "/x/plugins"), at("DELETE", "/x/plugins/a/b"), at("POST", "/x/plugins/x")), (Some(3), Some(3), Some(3)));
        // Exact means exact: no prefix match, no trailing slash.
        assert_eq!((at("GET", "/x/status/"), at("GET", "/x/statu"), at("GET", "/other"), at("GET", "")), (Some(5), Some(5), None, None));
        assert_eq!(find::<u8>(&[], "GET", "/"), None);
    }
}
