use std::marker::Send;
use std::mem::ManuallyDrop;

use async_condvar_fair::Condvar;
use parking_lot::Mutex;

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
    use test_case::test_case;

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

    #[test_case(vec![0, 0], 1, vec![1, 0] ; "two empty")]
    #[test_case(vec![0, 0], 1, vec![1, 1] ; "two empty, want both")]
    #[test_case(vec![4], 1, vec![6] ; "too many")]
    #[test_case(vec![0], 1, vec![1] ; "one empty")]
    #[test_case(vec![4], 0, vec![1] ; "no objs")]
    #[test_log::test]
    fn test_pools_one_empty_blocks(sizes: Vec<usize>, num_objs: usize, wants: Vec<usize>) {
        let pool = Pools::<String>::new(sizes.clone(), repeat("obj".to_owned()).take(num_objs));
        check_pending(pool.get(wants.clone())).unwrap()
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
