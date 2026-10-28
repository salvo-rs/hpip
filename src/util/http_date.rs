use chrono::{DateTime, Utc};

/// Date conditions are single-valued; repeated lines are an invalid list of dates.
pub(crate) fn request_date(req: &salvo::Request, name: &str) -> Option<DateTime<Utc>> {
    let mut values = req.headers().get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    DateTime::parse_from_rfc2822(value.to_str().ok()?)
        .ok()
        .map(|date| date.with_timezone(&Utc))
}
