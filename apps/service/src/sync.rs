//! ロックの取得ヘルパー。
//!
//! `std::sync::Mutex` はロックを持ったまま panic すると poison され、以後の `lock().unwrap()` がすべて panic する。
//! 常駐サービスでは、1 つのタスクの失敗が全ハンドラの連鎖的な失敗にならないよう、poison されていても
//! 中身をそのまま使う（保護している状態は単純な値で、途中で壊れても次の更新で上書きされる）。
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub trait LockExt<T> {
    fn lock_ok(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_ok(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub trait RwLockExt<T> {
    fn read_ok(&self) -> RwLockReadGuard<'_, T>;
    fn write_ok(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    fn read_ok(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write_ok(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_poisoned_mutex_is_still_usable() {
        let m = Arc::new(Mutex::new(1));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("ロック保持中に panic");
        })
        .join();
        assert!(m.lock().is_err(), "poison されている");
        *m.lock_ok() += 1; // それでも使える
        assert_eq!(*m.lock_ok(), 2);
    }

    #[test]
    fn a_poisoned_rwlock_is_still_usable() {
        let l = Arc::new(RwLock::new(String::from("a")));
        let l2 = l.clone();
        let _ = std::thread::spawn(move || {
            let _g = l2.write().unwrap();
            panic!("書き込みロック保持中に panic");
        })
        .join();
        assert!(l.read().is_err());
        l.write_ok().push('b');
        assert_eq!(*l.read_ok(), "ab");
    }
}
