use serde::Serialize;

/// A type to send weakly typed plots to plotly.
#[derive(Serialize, Clone)]
pub struct Value(pub serde_json::Value);

impl Value {
    pub fn boxed(self) -> Box<dyn plotly::Trace> {
        Box::new(self)
    }
}

impl plotly::Trace for Value {
    fn to_json(&self) -> String {
        serde_json::to_string(&self.0).unwrap()
    }
}
