//! Forecasting and anomaly detection (architecture §5, §6).

pub mod anomaly;
pub mod forecast;

pub use anomaly::{detect_anomalies_once, AnomalyMethod, AnomalyResult};
pub use forecast::{forecast_series, ForecastPoint, ForecastRequest, ForecastResponse};
