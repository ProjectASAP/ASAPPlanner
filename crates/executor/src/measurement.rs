use serde::{Deserialize, Serialize};
use std::ops::Add;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub value: f64,
}

impl Measurement {
    pub fn new(value: f64) -> Self {
        Self { value }
    }
}

impl Add for Measurement {
    type Output = Measurement;

    fn add(self, other: Measurement) -> Measurement {
        Measurement::new(self.value + other.value)
    }
}

impl Add for &Measurement {
    type Output = Measurement;

    fn add(self, other: &Measurement) -> Measurement {
        Measurement::new(self.value + other.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_measurement_creation() {
        let measurement = Measurement::new(42.5);
        assert_eq!(measurement.value, 42.5);
    }

    #[test]
    fn test_measurement_addition() {
        let m1 = Measurement::new(10.0);
        let m2 = Measurement::new(20.0);
        let result = m1 + m2;
        assert_eq!(result.value, 30.0);
    }
}
