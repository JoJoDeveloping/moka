use parking_lot::RwLock;
use std::{
    any::{Any, TypeId},
    hash::{BuildHasher, Hash},
    sync::Arc,
};
use triomphe::Arc as TrioArc;

use super::OptionallyNone;

const WAITER_MAP_NUM_SEGMENTS: usize = 64;

type ErrorObject = Arc<dyn Any + Send + Sync + 'static>;
type WaiterValue<V> = Option<Result<V, ErrorObject>>;
type Waiter<V> = TrioArc<RwLock<WaiterValue<V>>>;

pub(crate) enum InitResult<V, E> {
    Initialized(V),
    ReadExisting(V),
    InitErr(Arc<E>),
}

pub(crate) struct ValueInitializer<K, V, S> {
    // TypeId is the type ID of the concrete error type of generic type E in
    // try_init_or_read(). We use the type ID as a part of the key to ensure that
    // we can always downcast the trait object ErrorObject (in Waiter<V>) into
    // its concrete type.
    waiters: crate::cht::SegmentedHashMap<(Arc<K>, TypeId), Waiter<V>, S>,
}

impl<K, V, S> ValueInitializer<K, V, S>
where
    K: Eq + Hash,
    V: Clone,
    S: BuildHasher,
{
    pub(crate) fn with_hasher(hasher: S) -> Self {
        Self {
            waiters: crate::cht::SegmentedHashMap::with_num_segments_and_hasher(
                WAITER_MAP_NUM_SEGMENTS,
                hasher,
            ),
        }
    }

    /// # Panics
    /// Panics if the `init` future has been panicked.
    pub(crate) fn init_or_read(&self, key: Arc<K>, init: impl FnOnce() -> V) -> InitResult<V, ()> {
        // This closure will be called after the init closure has returned a value.
        // It will convert the returned value (from init) into an InitResult.
        let post_init = |_key, value: V, lock: &mut WaiterValue<V>| {
            *lock = Some(Ok(value.clone()));
            InitResult::Initialized(value)
        };

        let type_id = TypeId::of::<()>();
        self.do_try_init(&key, type_id, init, post_init)
    }

    /// # Panics
    /// Panics if the `init` future has been panicked.
    pub(crate) fn try_init_or_read<F, E>(&self, key: Arc<K>, init: F) -> InitResult<V, E>
    where
        F: FnOnce() -> Result<V, E>,
        E: Send + Sync + 'static,
    {
        let type_id = TypeId::of::<E>();

        // This closure will be called after the init closure has returned a value.
        // It will convert the returned value (from init) into an InitResult.
        let post_init = |key, value: Result<V, E>, lock: &mut WaiterValue<V>| match value {
            Ok(value) => {
                *lock = Some(Ok(value.clone()));
                InitResult::Initialized(value)
            }
            Err(e) => {
                let err: ErrorObject = Arc::new(e);
                *lock = Some(Err(Arc::clone(&err)));
                self.remove_waiter(key, type_id);
                InitResult::InitErr(err.downcast().unwrap())
            }
        };

        self.do_try_init(&key, type_id, init, post_init)
    }

    /// # Panics
    /// Panics if the `init` future has been panicked.
    pub(super) fn optionally_init_or_read<F>(
        &self,
        key: Arc<K>,
        init: F,
    ) -> InitResult<V, OptionallyNone>
    where
        F: FnOnce() -> Option<V>,
    {
        let type_id = TypeId::of::<OptionallyNone>();

        // This closure will be called after the init closure has returned a value.
        // It will convert the returned value (from init) into an InitResult.
        let post_init = |key, value: Option<V>, lock: &mut WaiterValue<V>| match value {
            Some(value) => {
                *lock = Some(Ok(value.clone()));
                InitResult::Initialized(value)
            }
            None => {
                // `value` can be either `Some` or `None`. For `None` case, without
                // change the existing API too much, we will need to convert `None`
                // to Arc<E> here. `Infalliable` could not be instantiated. So it
                // might be good to use an empty struct to indicate the error type.
                let err: ErrorObject = Arc::new(OptionallyNone);
                *lock = Some(Err(Arc::clone(&err)));
                self.remove_waiter(key, type_id);
                InitResult::InitErr(err.downcast().unwrap())
            }
        };

        self.do_try_init(&key, type_id, init, post_init)
    }

    /// # Panics
    /// Panics if the `init` future has been panicked.
    fn do_try_init<'a, F, O, C, E>(
        &self,
        key: &'a Arc<K>,
        type_id: TypeId,
        init: F,
        mut post_init: C,
    ) -> InitResult<V, E>
    where
        F: FnOnce() -> O,
        C: FnMut(&'a Arc<K>, O, &mut WaiterValue<V>) -> InitResult<V, E>,
        E: Send + Sync + 'static,
    {
        use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
        use InitResult::*;

        const MAX_RETRIES: usize = 200;
        let mut retries = 0;

        loop {
            let waiter = TrioArc::new(RwLock::new(None));
            let mut lock = waiter.write();

            match self.try_insert_waiter(key, type_id, &waiter) {
                None => {
                    // Our waiter was inserted. Let's resolve the init future.
                    // Catching panic is safe here as we do not try to resolve the future again.
                    match catch_unwind(AssertUnwindSafe(init)) {
                        // Resolved.
                        Ok(value) => return post_init(key, value, &mut lock),
                        // Panicked.
                        Err(payload) => {
                            *lock = None;
                            // Remove the waiter so that others can retry.
                            self.remove_waiter(key, type_id);
                            resume_unwind(payload);
                        } // The write lock will be unlocked here.
                    }
                }
                Some(res) => {
                    // Somebody else's waiter already exists. Drop our write lock and wait
                    // for a read lock to become available.
                    std::mem::drop(lock);
                    match &*res.read() {
                        Some(Ok(value)) => return ReadExisting(value.clone()),
                        Some(Err(e)) => return InitErr(Arc::clone(e).downcast().unwrap()),
                        // None means somebody else's init closure has been panicked.
                        None => {
                            retries += 1;
                            if retries < MAX_RETRIES {
                                // Retry from the beginning.
                                continue;
                            } else {
                                panic!(
                                    "Too many retries. Tried to read the return value from the `init` \
                                closure but failed {} times. Maybe the `init` kept panicking?",
                                    retries
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[inline]
    pub(crate) fn remove_waiter(&self, key: &Arc<K>, type_id: TypeId) {
        let (cht_key, hash) = self.cht_key_hash(key, type_id);
        self.waiters.remove(hash, |k| k == &cht_key);
    }

    #[inline]
    fn try_insert_waiter(
        &self,
        key: &Arc<K>,
        type_id: TypeId,
        waiter: &Waiter<V>,
    ) -> Option<Waiter<V>> {
        let (cht_key, hash) = self.cht_key_hash(key, type_id);
        let waiter = TrioArc::clone(waiter);
        self.waiters.insert_if_not_present(cht_key, hash, waiter)
    }

    #[inline]
    fn cht_key_hash(&self, key: &Arc<K>, type_id: TypeId) -> ((Arc<K>, TypeId), u64) {
        let cht_key = (Arc::clone(key), type_id);
        let hash = self.waiters.hash(&cht_key);
        (cht_key, hash)
    }
}
