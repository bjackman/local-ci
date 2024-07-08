use std::marker::Send;
use std::{mem::ManuallyDrop, ops::Deref};

use async_condvar_fair::Condvar;
use parking_lot::Mutex;
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
        let mut objs = self.objs.lock();
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
        let mut objs = self.objs.lock();
        objs.push(obj);
    }
}

#[derive(Debug)]
// Collection of shared resources, consisting of some sub-pools of "tokens" and a singular pool of
// objects. The user can request a given number tokens and one object - the tokens are just
// implemented as counters while the objects are actually returned when requested.
pub struct Pools<T> {
    cond: Condvar,
    resources: Mutex<(Vec<usize>, Vec<T>)>,
}

impl<T: Send> Pools<T> {
    // Create a collection of pools where sizes specifies the initial number of tokens in each
    // pool.
    pub fn new<I, J>(token_counts: I, objs: J) -> Self
    where
        I: IntoIterator<Item = usize>,
        J: IntoIterator<Item = T>,
    {
        Self {
            cond: Condvar::new(),
            resources: Mutex::new((
                token_counts.into_iter().collect(),
                objs.into_iter().collect(),
            )),
        }
    }

    // Get the specified number of tokens from each of the pools, indexes match
    // the indexes used in new. Panics if the size of counts differs from the number of pools.
    // The tokens are held until you drop the returned value.
    pub async fn get<I: IntoIterator<Item = usize>>(&self, token_counts: I) -> Resources<T> {
        let wants: Vec<_> = token_counts.into_iter().collect();
        let mut guard = self.resources.lock();
        loop {
            let (ref mut avail_token_counts, ref mut objs) = *guard;
            assert!(wants.len() == avail_token_counts.len());
            if avail_token_counts
                .iter()
                .zip(wants.iter())
                .all(|(have, want)| have >= want)
                && !objs.is_empty()
            {
                for (i, want) in wants.iter().enumerate() {
                    avail_token_counts[i] -= want;
                }
                let obj = objs.pop().unwrap();

                return Resources {
                    token_counts: ManuallyDrop::new(wants),
                    obj: ManuallyDrop::new(obj),
                    pools: self,
                };
            }
            guard = self.cond.wait(guard).await;
        }
    }

    fn put<I: IntoIterator<Item = usize>>(&self, token_counts: I, obj: T) {
        let token_counts: Vec<_> = token_counts.into_iter().collect();
        let mut guard = self.resources.lock();
        let (ref mut avail_token_counts, ref mut objs) = *guard;
        assert!(token_counts.len() == avail_token_counts.len());
        for (i, want) in token_counts.iter().enumerate() {
            avail_token_counts[i] += want;
        }
        objs.push(obj);
        // Note this is pretty inefficient, we are waking up every getter even though we can satisfy
        // at most one of them.
        self.cond.notify_all();
    }
}

#[derive(Debug)]
// Tokens taken from a Pools.
pub struct Resources<'a, T: Send> {
    token_counts: ManuallyDrop<Vec<usize>>,
    obj: ManuallyDrop<T>,
    pools: &'a Pools<T>,
}

impl<T: Send> Resources<'_, T> {
    pub fn obj(&self) -> &T {
        &self.obj
    }
}

impl<T: Send> Drop for Resources<'_, T> {
    fn drop(&mut self) {
        // SAFETY: This is safe as the fields are never accessed again.
        let (counts, obj) = unsafe {
            (
                ManuallyDrop::take(&mut self.token_counts),
                ManuallyDrop::take(&mut self.obj),
            )
        };
        self.pools.put(counts, obj)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::bail;
    use std::{
        iter::repeat,
        task::{Context, Poll},
    };

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
            Poll::Ready(res) => bail!("The future should be pending, but it produced {:?}", res),
        }
    }

    #[test_log::test]
    fn test_pool_empty_blocks() {
        let pool = Pool::<bool>::new([]);
        check_pending(pool.get()).expect("empty pool returned value");
    }

    #[test_log::test]
    fn test_pools_one_empty_blocks() {
        for (desc, sizes, num_objs, wants) in [
            ("one empty", vec![0], 1, vec![1]),
            ("two empty", vec![0, 0], 1, vec![1, 0]),
            ("two empty, want both", vec![0, 0], 1, vec![1, 1]),
            ("too many", vec![4], 1, vec![6]),
            ("no objs", vec![4], 0, vec![1]),
        ] {
            let pool = Pools::<String>::new(sizes.clone(), repeat("obj".to_owned()).take(num_objs));
            check_pending(pool.get(wants.clone()))
                .expect(format!("{}: {:?}.get({:?}) didn't block", desc, sizes, wants).as_str());
        }
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

    #[test_log::test(tokio::test)]
    async fn test_pools_get_some() {
        let pools = Pools::new([3, 4], ["obj1", "obj2"].map(|s| s.to_owned()));
        {
            let _tokens = pools.get(vec![1, 2]).await;
            check_pending(pools.get(vec![3, 0])).expect("returned too many tokens");
        }
        pools.get(vec![3, 0]).await;
    }
}
