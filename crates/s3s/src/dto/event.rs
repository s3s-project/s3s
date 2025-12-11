#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Event(String);

impl From<String> for Event {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for Event {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl From<Event> for String {
    fn from(value: Event) -> Self {
        value.0
    }
}
