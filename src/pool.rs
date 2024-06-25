use std::marker::Send;
use std::{mem::ManuallyDrop, ops::Deref, sync::Mutex};

use tokio::sync::{Semaphore, SemaphorePermit};

// Static collection of objects that can be temporarily allocated for mutually exclusive ownership.
#[derive(Debug)]
pub struct Pool<T> {
    // Note this is a NORMAL mutex not an async one. That means that you must not await while
    // holding it; this could lead to a deadlock. You can think of this a bit like a spinlock in
    // Linux. This is so that we can modify the vector in non-async code, so that we can call
    // Pool::put from the destructor of the PoolItem. This seems completely fucked up but actually
    // it's recommended by the tokio docs:
    // https://docs.rs/tokio/1.38.0/tokio/sync/struct.Mutex.html#which-kind-of-mutex-should-you-use
    objs: Mutex<Vec<T>>,
    // If you can get this semaphore, items is guaranteed not to be empty.
    // This is a kinda weird workaround for the fact that there's no equivalent
    // to a Go channel in tokio and no condition variables. This is actually
    // expected to block for a long time, so this is an async semaphore.
    sem: Semaphore,
    pub size: usize,
}

impl<T> Pool<T> {
    pub fn new<I>(objs: I) -> Self
    where
        I: IntoIterator<Item = T>,
    {
        let vec: Vec<T> = objs.into_iter().collect();
        let len = vec.len();
        Self {
            size: vec.len(),
            objs: Mutex::new(vec),
            sem: Semaphore::new(len),
        }
    }
}

#[derive(Debug)]
pub struct PoolItem<'a, T: Send> {
    // This ManuallyDrop sketchiness is to work around the fact that we want to move out of this
    // item back to the pool in drop. It means the field must be private.
    obj: ManuallyDrop<T>,
    _permit: SemaphorePermit<'a>,
    pool: &'a Pool<T>,
}

impl<T: Send> Deref for PoolItem<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.obj
    }
}

impl<T: Send> AsRef<T> for PoolItem<'_, T> {
    fn as_ref(&self) -> &T {
        &self.obj
    }
}

impl<T: Send> Drop for PoolItem<'_, T> {
    fn drop(&mut self) {
        // SAFETY: This is safe as the field is never accessed again.
        let obj = unsafe { ManuallyDrop::take(&mut self.obj) };
        self.pool.put(obj);
        // (Now we drop the semaphore permit, notifying waiters that obj is available).
    }
}

impl<T: Send> Pool<T> {
    // Get an item from the pool, you must call put on it later. It sucks that it's this easy to
    // leak items. I thought we could just return them on Drop but it seems to be impossible to
    // actually do this in drop, since we need to await the lock. We could just put the cleanup into
    // a background thread but this then makes a huge mess since we would need to be able to mutate
    // the PoolItem in put, but then it still needs to be a valid object in Drop::drop (since you
    // only get a mutable reference to the object being dropped, not the object itself). This means
    // you'd end up needing some sort of mutation synchronization in PoolItem, even though it's
    // logically an immutable type. Yuck yuck yuck.
    pub async fn get(&self) -> PoolItem<T> {
        let permit = self
            .sem
            .acquire()
            .await
            .expect("Pool bug: semaphore closed");
        let mut objs = self.objs.lock().unwrap();
        let obj = objs.pop().expect(
            "Pool empty when semaphore acquired . \
                This probably means a call to Pool::put was missed.",
        );
        PoolItem {
            obj: ManuallyDrop::new(obj),
            _permit: permit,
            pool: self,
        }
    }

    // Add an item to the pool.
    fn put(&self, obj: T) {
        let mut objs = self.objs.lock().unwrap();
        objs.push(obj);
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{ bail};
    use std::task::{Context, Poll};

    use futures::{pin_mut, task::noop_waker, Future};

    use super::*;

    // Assert that a future is blocked. Note that panicking directly in assertion helpers like this
    // is unhelpful because you lose line number debug. It seems the proper solution for that is to
    // make them macros instead of functions. My solution is instead to just return errors and then
    // .expect() them, because I don't know how to make macros.
    fn check_pending<F>(fut: F) -> anyhow::Result<()>
    where
        F: Future,
        <F as futures::Future>::Output: std::fmt::Debug,
    {
        pin_mut!(fut);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Poll the future before it completes
        match fut.as_mut().poll(&mut cx) {
            Poll::Pending => Ok(()),
            Poll::Ready(res) => bail!(
                "The future should be pending, but it produced {:?}",
                res
            ),
        }
    }

    #[test_log::test]
    fn test_empty_blocks() {
        let pool = Pool::<bool>::new([]);
        check_pending(pool.get()).expect("empty pool returned value");
    }

    #[test_log::test(tokio::test)]
    async fn test_get_some() {
        // I originally used ints here, then got really confused when the comiler let me
        // pool.put(*obj) over and over again. Then I realised it's because ints are Copy. It
        // doesn't really make any sense to have Pools of a Copy type so we test with Strings here.
        let pool = Pool::<String>::new(["one", "two", "three"].map(|s| s.to_owned()));
        // We don't actually functionally care about the order of the returned values, but
        //  - Stack order seems more cache-friendly
        //  - Asserting on the specific values is an easy way to check nothing insane is happening.
        {
            let obj3 = pool.get().await;
            assert_eq!(*obj3, "three");
            let obj2 = pool.get().await;
            assert_eq!(*obj2, "two");
            let obj1 = pool.get().await;
            assert_eq!(*obj1, "one");
            let blocked_get = pool.get();
            check_pending(blocked_get).expect("empty pool returned value");
        }
        let obj = pool.get().await;
        assert_eq!(*obj, "three");
    }
}
