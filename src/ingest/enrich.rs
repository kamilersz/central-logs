//! Enrichment: GeoIP, hostname tagging, severity normalization (architecture §2b.2).

use crate::store::appender::LogRow;

/// Optional GeoIP resolver. Behind the `geoip` feature flag.
#[cfg(feature = "geoip")]
pub struct GeoEnricher {
    reader: maxminddb::Reader<Vec<u8>>,
}

#[cfg(feature = "geoip")]
impl GeoEnricher {
    pub fn open(path: &std::path::Path) -> crate::Result<Self> {
        let reader = maxminddb::Reader::read_mmap(path)
            .or_else(|_| maxminddb::Reader::open_readfile(path))
            .map_err(|e| crate::Error::config(format!("geoip open: {e}")))?;
        Ok(Self { reader })
    }

    pub fn country(&self, ip: std::net::IpAddr) -> Option<String> {
        use maxminddb::geoip2::Country;
        let c: Country = self.reader.lookup(ip).ok()?;
        c.country.and_then(|c| c.iso_code).map(String::from)
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
        #[cfg(feature = "geoip")]
        geo: Option<std::sync::Arc<GeoEnricher>>,
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
