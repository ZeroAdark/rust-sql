//! `binding`'s unsafe step (rule R6, the module's raw home): a worker binding's Send promise.
#![allow(unsafe_code)]

use super::WorkerBinding;

unsafe impl Send for WorkerBinding<'_> {}
