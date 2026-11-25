//! cancel.rs
//! core::call::cancel
//!
//! Request cancellation for MCP notifications/cancelled.
//! The reader thread registers every tools/call id before dispatch, a cancel notice flips that
//! request's flag, and long walks poll the flag of the request their thread serves through a
//! thread-local scope that the batch pool carries into its workers.
//!

use serde_json::Value;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

// 0-1. Registry state ---------------------------------------------------------
// 진행 중 tools/call 의 (id 직렬화 키, 취소 플래그). 동시 호출 상한이 64라 Vec 선형 탐색.
static ACTIVE: Mutex<Vec<(String, Arc<AtomicBool>)>> = Mutex::new(Vec::new());

thread_local! {
    // 이 스레드가 지금 처리 중인 요청의 취소 플래그
    static CURRENT: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

// 등록 guard: 요청 처리가 끝나 drop 되면 레지스트리에서 빠진다
pub struct CancelRegistration {
    flag: Arc<AtomicBool>,
}
impl CancelRegistration {
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}
impl Drop for CancelRegistration {
    fn drop(&mut self) {
        let mut active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
        active.retain(|(_, flag)| !Arc::ptr_eq(flag, &self.flag));
    }
}
// 스레드 범위 guard: drop 되면 이전 플래그를 되돌린다(중첩 진입 안전)
pub struct CancelScope {
    previous: Option<Arc<AtomicBool>>,
}
impl Drop for CancelScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|current| current.replace(previous));
    }
}

// 1. Register and cancel ------------------------------------------------------
// JSON-RPC id 는 타입까지 구분하므로(5 와 "5") 직렬화 문자열을 키로 쓴다.
pub fn register_request(id: &Value) -> CancelRegistration {
    let flag = Arc::new(AtomicBool::new(false));
    let mut active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
    active.push((id.to_string(), Arc::clone(&flag)));
    CancelRegistration { flag }
}
// 모르는 id(이미 끝난 요청 포함)는 무시한다. 같은 id 가 여럿 진행 중이면 모두 취소한다.
pub fn cancel_request(id: &Value) {
    let key = id.to_string();
    let active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
    for (_, flag) in active.iter().filter(|(entry, _)| *entry == key) {
        flag.store(true, Ordering::Relaxed);
    }
}

// 2. Thread scope -------------------------------------------------------------
pub fn enter_cancel_scope(flag: Option<Arc<AtomicBool>>) -> CancelScope {
    let previous = CURRENT.with(|current| current.replace(flag));
    CancelScope { previous }
}
pub fn current_cancel_flag() -> Option<Arc<AtomicBool>> {
    CURRENT.with(|current| current.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cancel_flips_only_the_matching_request() {
        let first = register_request(&json!(9_001));
        let second = register_request(&json!("9001"));
        cancel_request(&json!(9_001));
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
    }

    #[test]
    fn finished_request_ignores_late_cancel() {
        let registration = register_request(&json!("late-cancel"));
        let flag = registration.flag();
        drop(registration);
        cancel_request(&json!("late-cancel"));
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn scope_restores_previous_flag() {
        assert!(current_cancel_flag().is_none());
        let outer = Arc::new(AtomicBool::new(false));
        let outer_scope = enter_cancel_scope(Some(Arc::clone(&outer)));
        let inner_scope = enter_cancel_scope(Some(Arc::new(AtomicBool::new(true))));
        assert!(current_cancel_flag().is_some_and(|flag| flag.load(Ordering::Relaxed)));
        drop(inner_scope);
        assert!(current_cancel_flag().is_some_and(|flag| Arc::ptr_eq(&flag, &outer)));
        drop(outer_scope);
        assert!(current_cancel_flag().is_none());
    }
}
