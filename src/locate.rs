//! Where the app is, for Locate Me.
//!
//! The web demo asked the browser, which asks the OS. A native binary would
//! need CoreLocation or GeoClue for that, and on macOS CoreLocation means
//! shipping an app bundle with a usage description — so this asks a service
//! what the address it sees resolves to instead.
//!
//! That is a lookup over the network, and pressing the button sends this
//! machine's public address to [`ENDPOINT`]. Accuracy is roughly the city.
//! `OSM_SOUND_DEMO_HOME` short-circuits the whole thing, which is what to use
//! at a venue, offline, or when the answer should not depend on someone else's
//! database.

use std::time::Duration;

use crate::otherman::Error;

/// Returns `{"loc": "35.6895,139.6917", "city": "Tokyo", "country": "JP", ...}`.
const ENDPOINT: &str = "https://ipinfo.io/json";

/// Give up rather than leave the button looking stuck.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TIMEOUT: Duration = Duration::from_secs(10);

/// Set to `lat,lon` to answer without asking anyone.
pub const HOME_VAR: &str = "OSM_SOUND_DEMO_HOME";

#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub lat: f64,
    pub lon: f64,
    /// What to call the place in the status line.
    pub label: String,
}

/// Reads `OSM_SOUND_DEMO_HOME`, if it is set and usable.
pub fn home() -> Option<Located> {
    parse_home(&std::env::var(HOME_VAR).ok()?)
}

fn parse_home(value: &str) -> Option<Located> {
    let (lat, lon) = value.split_once(',')?;
    let lat: f64 = lat.trim().parse().ok()?;
    let lon: f64 = lon.trim().parse().ok()?;
    // A latitude outside the range is a swapped pair, not a location.
    (-90.0..=90.0).contains(&lat).then(|| Located {
        lat,
        lon,
        label: HOME_VAR.to_owned(),
    })
}

/// Asks the service where this machine appears to be. Blocking; call it off the
/// UI thread.
pub fn lookup() -> Result<Located, Error> {
    let response: serde_json::Value = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(TIMEOUT))
        .build()
        .new_agent()
        .get(ENDPOINT)
        .call()?
        .body_mut()
        .read_json()?;
    parse_lookup(&response).ok_or_else(|| format!("{ENDPOINT} returned no location").into())
}

fn parse_lookup(response: &serde_json::Value) -> Option<Located> {
    let (lat, lon) = response.get("loc")?.as_str()?.split_once(',')?;
    let lat: f64 = lat.trim().parse().ok()?;
    let lon: f64 = lon.trim().parse().ok()?;
    if !(-90.0..=90.0).contains(&lat) {
        return None;
    }

    let text = |key: &str| {
        response
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let label = match (text("city"), text("country")) {
        (Some(city), Some(country)) => format!("{city}, {country}"),
        (Some(city), None) => city,
        (None, Some(country)) => country,
        (None, None) => format!("{lat:.4}, {lon:.4}"),
    };
    Some(Located { lat, lon, label })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_reads_a_coordinate_pair() {
        let parsed = parse_home("35.68,139.76").expect("a pair");
        assert_eq!((parsed.lat, parsed.lon), (35.68, 139.76));
        assert_eq!(parse_home(" -1.28 , 36.81 ").map(|p| p.lat), Some(-1.28));

        assert_eq!(parse_home("nonsense"), None);
        assert_eq!(parse_home("35.68"), None);
        // Latitude out of range means the pair is the wrong way round.
        assert_eq!(parse_home("139.76,35.68"), None);
    }

    #[test]
    fn a_lookup_reads_the_location_and_names_it() {
        let response = serde_json::json!({
            "ip": "203.0.113.1",
            "city": "Sapporo",
            "region": "Hokkaido",
            "country": "JP",
            "loc": "43.0621,141.3544",
        });
        let located = parse_lookup(&response).expect("a location");
        assert_eq!((located.lat, located.lon), (43.0621, 141.3544));
        assert_eq!(located.label, "Sapporo, JP");
    }

    #[test]
    fn a_lookup_without_a_city_still_names_the_place() {
        let unnamed = serde_json::json!({ "loc": "43.0621,141.3544" });
        assert_eq!(
            parse_lookup(&unnamed).expect("a location").label,
            "43.0621, 141.3544"
        );

        let country_only = serde_json::json!({ "loc": "43.0621,141.3544", "country": "JP" });
        assert_eq!(parse_lookup(&country_only).expect("a location").label, "JP");
    }

    #[test]
    fn a_response_without_a_usable_location_is_rejected() {
        // What the service returns when it cannot place an address, plus the
        // shapes that would otherwise parse into nonsense.
        assert_eq!(
            parse_lookup(&serde_json::json!({ "ip": "203.0.113.1" })),
            None
        );
        assert_eq!(parse_lookup(&serde_json::json!({ "loc": "" })), None);
        assert_eq!(parse_lookup(&serde_json::json!({ "loc": "43.0621" })), None);
        assert_eq!(
            parse_lookup(&serde_json::json!({ "loc": "north,east" })),
            None
        );
        assert_eq!(
            parse_lookup(&serde_json::json!({ "loc": "141.3544,43.0621" })),
            None
        );
    }

    /// Opt-in: the real service, since its response shape is the thing that
    /// could change under us.
    #[test]
    fn the_real_service_answers_with_a_location() {
        if std::env::var_os("OSM_SOUND_DEMO_NETWORK_TESTS").is_none() {
            eprintln!("skipped: set OSM_SOUND_DEMO_NETWORK_TESTS=1 to run");
            return;
        }
        let located = lookup().expect("looking up this machine");
        eprintln!("{located:?}");
        assert!((-90.0..=90.0).contains(&located.lat));
        assert!((-180.0..=180.0).contains(&located.lon));
        assert!(!located.label.is_empty());
    }
}
