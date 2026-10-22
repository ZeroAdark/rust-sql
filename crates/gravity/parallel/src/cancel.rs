use pgsync::Mutex;

use gravity_driver::Error;

#[derive(Debug, PartialEq, Eq)]
pub struct FirstError {
    pub claim_lo: u64,
    pub ordinal: u16,
    pub error: Error,
}

pub struct ErrorFunnel {
    first: Mutex<Option<FirstError>>,
}

impl Default for ErrorFunnel {
    fn default() -> ErrorFunnel {
        ErrorFunnel::new()
    }
}

impl ErrorFunnel {
    pub const fn new() -> ErrorFunnel {
        ErrorFunnel { first: Mutex::new(None) }
    }

    pub fn offer(&self, claim_lo: u64, ordinal: u16, error: Error) {
        let mut g = self.first.lock().unwrap_or_else(|e| e.into_inner());
        let better = match &*g {
            None => true,
            Some(f) => (claim_lo, ordinal) < (f.claim_lo, f.ordinal),
        };
        if better {
            *g = Some(FirstError { claim_lo, ordinal, error });
        }
    }

    pub fn take(&self) -> Option<FirstError> {
        self.first.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    pub fn is_set(&self) -> bool {
        self.first.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
}
