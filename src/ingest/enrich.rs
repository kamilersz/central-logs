//! Enrichment: GeoIP, hostname tagging, severity normalization (architecture §2b.2).

#[cfg(feature = "geoip")]
use std::sync::Arc;

use crate::store::appender::LogRow;

/// GeoIP resolution result: ISO country code, most-specific subdivision
/// (state/province ISO code, e.g. `JK` for Jakarta) and city name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Geo {
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
}

/// Optional GeoIP resolver. Behind the `geoip` feature flag.
///
/// Works with either a GeoLite2-City database (country + region + city) or a
/// GeoLite2-Country database (country only) — the City decode is attempted
/// first and falls back to Country, so both DB flavors are accepted.
#[cfg(feature = "geoip")]
pub struct GeoEnricher {
    reader: maxminddb::Reader<Vec<u8>>,
}

#[cfg(feature = "geoip")]
impl GeoEnricher {
    pub fn open(path: &std::path::Path) -> crate::Result<Self> {
        let reader = maxminddb::Reader::open_readfile(path)
            .map_err(|e| crate::Error::config(format!("geoip open: {e}")))?;
        Ok(Self { reader })
    }

    pub fn lookup(&self, ip: std::net::IpAddr) -> Geo {
        let mut out = Geo::default();
        let Ok(result) = self.reader.lookup(ip) else {
            return out;
        };
        // City DB: country + most-specific subdivision + city name.
        if let Ok(Some(city)) = result.decode::<maxminddb::geoip2::City>() {
            out.country = city.country.iso_code.map(String::from);
            out.region = city
                .subdivisions
                .last()
                .and_then(|s| s.iso_code)
                .map(String::from);
            out.city = city.city.names.english.map(String::from);
            if out.country.is_some() {
                return out;
            }
        }
        // Country DB (or a City miss): country code only.
        if let Ok(Some(c)) = result.decode::<maxminddb::geoip2::Country>() {
            out.country = c.country.iso_code.map(String::from);
        }
        out
    }

    pub fn country(&self, ip: std::net::IpAddr) -> Option<String> {
        self.lookup(ip).country
    }
}

/// Common enricher. Cheap, runs on every parsed row.
pub struct Enricher {
    #[cfg(feature = "geoip")]
    pub geo: Option<Arc<GeoEnricher>>,
    pub default_service: String,
    pub default_host: String,
}

impl Enricher {
    pub fn new(
        #[cfg(feature = "geoip")] geo: Option<std::sync::Arc<GeoEnricher>>,
        default_service: &str,
        default_host: &str,
    ) -> Self {
        Self {
            #[cfg(feature = "geoip")]
            geo,
            default_service: default_service.to_string(),
            default_host: default_host.to_string(),
        }
    }
    /// Enrich a parsed row in place. The `source_ip` (if any) drives GeoIP.
    pub fn enrich(&self, row: &mut LogRow, source_ip: Option<std::net::IpAddr>) {
        if row.service.is_empty() {
            row.service = self.default_service.clone();
        }
        if row.source_host.is_empty() {
            row.source_host = self.default_host.clone();
        }
        if row.level.is_empty() {
            row.level = "info".into();
        }
        #[cfg(feature = "geoip")]
        if let Some(geo) = &self.geo {
            if let Some(ip) = source_ip {
                if let Some(country) = geo.country(ip) {
                    row.geo_country = country;
                }
            }
        }
        let _ = source_ip;
    }
}

#[cfg(all(test, feature = "geoip"))]
mod tests {
    use super::*;

    #[test]
    fn open_missing_db_is_a_clean_config_error() {
        match GeoEnricher::open(std::path::Path::new("/nonexistent/GeoLite2-City.mmdb")) {
            Ok(_) => panic!("missing DB must not open"),
            Err(e) => assert!(e.to_string().contains("geoip open")),
        }
    }

    #[test]
    fn real_db_resolves_country_region_city() {
        // Runs only when ./geoip/GeoLite2-City.mmdb exists (fetched via
        // scripts/fetch-geolite.sh or the jsdelivr mirror); skipped otherwise
        // so CI without the 64MB file stays green. Tests run with the crate
        // root as CWD, so the relative path resolves.
        let path = std::path::Path::new("geoip/GeoLite2-City.mmdb");
        if !path.exists() {
            eprintln!("skipping: geoip/GeoLite2-City.mmdb not present");
            return;
        }
        let geo = GeoEnricher::open(path).expect("bundled DB must open");
        // Google Public DNS: stable US fixture address (anycast: country
        // only, no city — that is correct MaxMind data, not a decode bug).
        let g = geo.lookup("8.8.8.8".parse().unwrap());
        assert_eq!(g.country.as_deref(), Some("US"));
        // Full record: country + state ISO + city name.
        let g = geo.lookup("103.86.96.100".parse().unwrap());
        assert_eq!(g.country.as_deref(), Some("AU"));
        assert_eq!(g.region.as_deref(), Some("NSW"));
        assert_eq!(g.city.as_deref(), Some("Sydney"));
        let g = geo.lookup("36.81.183.29".parse().unwrap());
        assert_eq!(g.country.as_deref(), Some("ID"));
        assert_eq!(g.region.as_deref(), Some("JI"));
        assert_eq!(g.city.as_deref(), Some("Surabaya"));
        // Loopback is not in the DB → empty Geo (falls back to envelope cc).
        let loopback = geo.lookup("127.0.0.1".parse().unwrap());
        assert_eq!(loopback, Geo::default());
    }

    #[test]
    fn empty_geo_leaves_country_blank() {
        // No DB → Geo::default → downstream falls back to envelope `cc`.
        let g = Geo::default();
        assert!(g.country.is_none());
        assert!(g.region.is_none());
        assert!(g.city.is_none());
    }
}
