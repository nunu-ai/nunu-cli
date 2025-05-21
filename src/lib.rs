use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum RustTemplateError {
    #[error("You can't add 7 to something.")]
    CannotAddSeven,
}

pub fn add_safe(a: i32, b: i32) -> Result<i32, RustTemplateError> {
    if b == 7 {
        return Err(RustTemplateError::CannotAddSeven);
    }
    Ok(a + b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_safe() {
        assert_eq!(add_safe(1, 2), Ok(3));
    }

    #[test]
    fn test_add_safe_error() {
        assert_eq!(add_safe(1, 7), Err(RustTemplateError::CannotAddSeven));
    }
}
