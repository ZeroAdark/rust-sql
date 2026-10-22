use core::cell::Cell;

std::thread_local! {
    static EXIT_ON_ANY_ERROR: Cell<bool> = const { Cell::new(false) };
}

pub fn exit_on_any_error() -> bool {
    EXIT_ON_ANY_ERROR.get()
}

pub fn set_exit_on_any_error(value: bool) {
    EXIT_ON_ANY_ERROR.set(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PgError, ERROR, FATAL, NOTICE, PANIC};

    #[test]
    fn exit_on_error_promotes_only_errors_and_is_backend_local() {
        assert_eq!(PgError::error("before").level, ERROR);
        set_exit_on_any_error(true);
        let error =
            PgError::error("division by zero").with_sqlstate(crate::ERRCODE_DIVISION_BY_ZERO);
        let notice = PgError::new(NOTICE, "notice");
        let panic = PgError::new(PANIC, "panic");
        let other = std::thread::spawn(|| PgError::error("other backend"))
            .join()
            .unwrap();
        set_exit_on_any_error(false);
        assert_eq!(error.level, FATAL);
        assert_eq!(error.sqlstate, crate::ERRCODE_DIVISION_BY_ZERO);
        assert_eq!(error.message, "division by zero");
        assert_eq!(notice.level, NOTICE);
        assert_eq!(panic.level, PANIC);
        assert_eq!(other.level, ERROR);
        assert_eq!(PgError::error("after").level, ERROR);
    }
}
