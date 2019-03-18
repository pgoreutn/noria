use common::SizeOf;
use fnv::FnvBuildHasher;
use prelude::*;
use std::borrow::Cow;
use rand::{Rng, ThreadRng};
use std::sync::Arc;
use std::sync::Mutex;
use std::collections::HashMap;

/// Allocate a new end-user facing result table.
pub(crate) fn new(srmap: bool, cols: usize, key: &[usize], uid: usize) -> (SingleReadHandle, WriteHandle) {
    new_inner(srmap, cols, key, None, uid)
}

/// Allocate a new partially materialized end-user facing result table.
///
/// Misses in this table will call `trigger` to populate the entry, and retry until successful.
pub(crate) fn new_partial<F>(
    srmap: bool,
    cols: usize,
    key: &[usize],
    trigger: F,
    uid: usize,
) -> (SingleReadHandle, WriteHandle)
where
    F: Fn(&[DataType], Option<usize>) + 'static + Send + Sync,
{
    new_inner(srmap, cols, key, Some(Arc::new(trigger)), uid)
}

fn new_inner(
    srmap: bool,
    cols: usize,
    key: &[usize],
    trigger: Option<Arc<Fn(&[DataType], Option<usize>) + Send + Sync>>,
    uid: usize,
) -> (SingleReadHandle, WriteHandle) {
    let contiguous = {
        let mut contiguous = true;
        let mut last = None;
        for &k in key {
            if let Some(last) = last {
                if k != last + 1 {
                    contiguous = false;
                    break;
                }
            }
            last = Some(k);
        }
        contiguous
    };

    let mut srmap = true;

    macro_rules! make_srmap {
    ($variant:tt) => {{
            use srmap;
            let (r, w) = srmap::construct(-1);
            (multir_sr::Handle::$variant(r), multiw_sr::Handle::$variant(w))
        }};
    }

    macro_rules! make {
        ($variant:tt) => {{
            use evmap;
            let (r, w) = evmap::Options::default()
                .with_meta(-1)
                .with_hasher(FnvBuildHasher::default())
                .construct();
            (multir::Handle::$variant(r), multiw::Handle::$variant(w))
        }};
    }

    if srmap {
        let (r, w) = match (key.len(), srmap) {
            (0, _) => unreachable!(),
            (1, true) => make_srmap!(SingleSR),
            (2, true) => make_srmap!(DoubleSR),
            (_, true) => make_srmap!(ManySR),
            (_, false) => unreachable!(),
        };

        let w = WriteHandle {
            partial: trigger.is_some(),
            handle: None,
            handleSR: Some(w),
            srmap: true,
            key: Vec::from(key),
            cols: cols,
            contiguous,
            mem_size: 0,
            uid: uid,
        };

        let r = SingleReadHandle {
            handle: None,
            handleSR: Some(r),
            srmap: true,
            trigger: trigger,
            key: Vec::from(key),
            uid: uid
        };

        (r, w)

    } else {
        let (r, w) = match (key.len(), srmap) {
            (0, _) => unreachable!(),
            (1, false) => make!(Single),
            (2, false) => make!(Double),
            (_, false) => unreachable!(),
            (_, true) => unreachable!(),
        };

        let w = WriteHandle {
            partial: trigger.is_some(),
            handle: Some(w),
            handleSR: None,
            srmap: false,
            key: Vec::from(key),
            cols: cols,
            contiguous,
            mem_size: 0,
            uid: uid,
        };

        let r = SingleReadHandle {
            handle: Some(r),
            handleSR: None,
            srmap: false,
            trigger: trigger,
            key: Vec::from(key),
            uid: uid
        };

        (r, w)
    }
}

mod multir;
mod multiw;
mod multir_sr;
mod multiw_sr;

fn key_to_single<'a>(k: Key<'a>) -> Cow<'a, DataType> {
    assert_eq!(k.len(), 1);
    match k {
        Cow::Owned(mut k) => Cow::Owned(k.swap_remove(0)),
        Cow::Borrowed(k) => Cow::Borrowed(&k[0]),
    }
}

fn key_to_double<'a>(k: Key<'a>) -> Cow<'a, (DataType, DataType)> {
    assert_eq!(k.len(), 2);
    match k {
        Cow::Owned(k) => {
            let mut k = k.into_iter();
            let k1 = k.next().unwrap();
            let k2 = k.next().unwrap();
            Cow::Owned((k1, k2))
        }
        Cow::Borrowed(k) => Cow::Owned((k[0].clone(), k[1].clone())),
    }
}

pub(crate) struct WriteHandle {
    handle: Option<multiw::Handle>,
    handleSR: Option<multiw_sr::Handle>,
    srmap: bool,
    partial: bool,
    cols: usize,
    key: Vec<usize>,
    contiguous: bool,
    mem_size: usize,
    pub uid: usize
}

type Key<'a> = Cow<'a, [DataType]>;
pub(crate) struct MutWriteHandleEntry<'a> {
    handle: &'a mut WriteHandle,
    key: Key<'a>,
}
pub(crate) struct WriteHandleEntry<'a> {
    handle: &'a mut WriteHandle,
    key: Key<'a>,
}

impl<'a> MutWriteHandleEntry<'a> {
    pub fn mark_filled(&mut self) {
        println!("markfilled 1");
        let handle = &mut self.handle.handleSR;
        match handle {
            Some(hand) => {
                println!("markfilled 2");
                if let Some((None, _)) = hand
                    .meta_get_and(Cow::Borrowed(&*self.key), |rs| rs.is_empty())
                {
                    hand.clear(Cow::Borrowed(&*self.key));
                    println!("markfilled 3");
                } else {
                    unreachable!("attempted to fill already-filled key");
                }
            },
            None => {}
        }
    }

    pub fn mark_hole(&mut self) {
        let handle = &mut self.handle.handleSR;
        println!("mark hole");
        match handle {
            Some(hand) => {
                let size = hand
                    .meta_get_and(Cow::Borrowed(&*self.key), |rs| {
                        rs.iter().map(|r| r.deep_size_of()).sum()
                    })
                    .map(|r| r.0.unwrap_or(0))
                    .unwrap_or(0);
                self.handle.mem_size = self.handle.mem_size.checked_sub(size as usize).unwrap();
                hand.empty(Cow::Borrowed(&*self.key))
            },
            None => {}
        }
    }
}

impl<'a> WriteHandleEntry<'a> {
    pub(crate) fn try_find_and<F, T>(&mut
         self, mut then: F) -> Result<(Option<T>, i64), ()>
    where
        F: FnMut(&[Vec<DataType>]) -> T,
    {

        match &self.handle.handleSR {
            Some(handleSR) => {
                handleSR.meta_get_and(self.key.clone(), &mut then).ok_or(())
            },
            None => {Err(())}
        }
        // match &self.handle.handle {
        //     Some(handle) => {
        //         handle.meta_get_and(self.key.clone(), &mut then).ok_or(())
        //     },
        //     None => {
        //         match &self.handle.handleSR {
        //             Some(handleSR) => {
        //                 handleSR.meta_get_and(self.key.clone(), &mut then).ok_or(())
        //             },
        //             None => {Err(())}
        //         }
        //
        //     }
        // }
    }
}

fn key_from_record<'a, R>(key: &[usize], contiguous: bool, record: R) -> Key<'a>
where
    R: Into<Cow<'a, [DataType]>>,
{
    match record.into() {
        Cow::Owned(mut record) => {
            let mut i = 0;
            let mut keep = key.into_iter().peekable();
            record.retain(|_| {
                i += 1;
                if let Some(&&next) = keep.peek() {
                    if next != i - 1 {
                        return false;
                    }
                } else {
                    return false;
                }

                assert_eq!(*keep.next().unwrap(), i - 1);
                true
            });
            Cow::Owned(record)
        }
        Cow::Borrowed(record) if contiguous => Cow::Borrowed(&record[key[0]..(key[0] + key.len())]),
        Cow::Borrowed(record) => Cow::Owned(key.iter().map(|&i| &record[i]).cloned().collect()),
    }
}

impl WriteHandle {

    pub(crate) fn clone_new_user(&mut self, r: &mut SingleReadHandle) -> Option<(SingleReadHandle, WriteHandle)> {
        if self.srmap {
            let handle = &mut self.handleSR;
            match handle {
                Some(hand) => {
                    let (uid, r_handle, w_handle) = hand.clone_new_user();
                    println!("CLONING NEW USER. uid: {}", uid);
                    let r = r.clone_new_user(r_handle, uid.clone());
                    let w =  WriteHandle {
                        handle: None,
                        handleSR: Some(w_handle),
                        srmap: true,
                        partial: self.partial.clone(),
                        cols: self.cols.clone(),
                        key: self.key.clone(),
                        contiguous: self.contiguous.clone(),
                        mem_size: self.mem_size.clone(),
                        uid: uid.clone()};
                    return Some((r, w));
                },
                None => {None}
            }
        } else {
            return None;
        }
    }
    

    pub(crate) fn clone_new_user_partial(&mut self, r: &mut SingleReadHandle, trigger: Option<Arc<Fn(&[DataType], Option<usize>) + Send + Sync>>) -> Option<(SingleReadHandle, WriteHandle)> {
        if self.srmap {
            let handle = &mut self.handleSR;
            match handle {
                Some(hand) => {
                    let (uid, r_handle, w_handle) = hand.clone_new_user();
                    println!("CLONING NEW USER. uid: {}", uid);
                    let r = r.clone_new_user_partial(r_handle, uid.clone(), trigger);
                    let w =  WriteHandle {
                        handle: None,
                        handleSR: Some(w_handle),
                        srmap: true,
                        partial: self.partial.clone(),
                        cols: self.cols.clone(),
                        key: self.key.clone(),
                        contiguous: self.contiguous.clone(),
                        mem_size: self.mem_size.clone(),
                        uid: uid.clone()};
                    return Some((r, w));
                },
                None => {None}
            }
        } else {
            return None;
        }
    }


    pub(crate) fn clone(&mut self, r: &mut SingleReadHandle) -> Option<(SingleReadHandle, WriteHandle)> {
        if self.srmap {
            let handle = &mut self.handleSR;
            match handle {
                Some(hand) => {
                    let w_handle = hand.clone();
                    match r.handleSR.clone() {
                        Some(rhand) => {
                            let w =  WriteHandle {
                                handle: None,
                                handleSR: Some(w_handle),
                                srmap: true,
                                partial: self.partial.clone(),
                                cols: self.cols.clone(),
                                key: self.key.clone(),
                                contiguous: self.contiguous.clone(),
                                mem_size: self.mem_size.clone(),
                                uid: self.uid.clone()};
                            return Some((r.clone(rhand.clone(), self.uid.clone()), w));
                        },
                        None => {None}
                    }
                },
                None => {None}
            }

        } else {
            return None;
        }
    }


    pub(crate) fn mut_with_key<'a, K>(&'a mut self, key: K) -> MutWriteHandleEntry<'a>
    where
        K: Into<Key<'a>>,
    {
        MutWriteHandleEntry {
            handle: self,
            key: key.into(),
        }
    }

    pub(crate) fn with_key<'a, K>(&'a mut self, key: K) -> WriteHandleEntry<'a>
    where
        K: Into<Key<'a>>,
    {
        WriteHandleEntry {
            handle: self,
            key: key.into(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn mut_entry_from_record<'a, R>(&'a mut self, record: R) -> MutWriteHandleEntry<'a>
    where
        R: Into<Cow<'a, [DataType]>>,
    {
        let key = key_from_record(&self.key[..], self.contiguous, record);
        self.mut_with_key(key)
    }

    pub(crate) fn entry_from_record<'a, R>(&'a mut self, record: R) -> WriteHandleEntry<'a>
    where
        R: Into<Cow<'a, [DataType]>>,
    {
        let key = key_from_record(&self.key[..], self.contiguous, record);
        self.with_key(key)
    }

    pub(crate) fn swap(&mut self) {
        if self.srmap {
            let handle = &mut self.handleSR;
            match handle {
                Some(hand) => { hand.refresh(); },
                None => {},
            }
        } else {
            let handle = &mut self.handle;
            match handle {
                Some(hand) => { hand.refresh(); },
                None => {},
            }
        }
    }

    /// Add a new set of records to the backlog.
    ///
    /// These will be made visible to readers after the next call to `swap()`.
    pub(crate) fn add<I>(&mut self, rs: I, id: Option<usize>)
    where
        I: IntoIterator<Item = Record>,
    {
        if self.srmap {
            let handle = &mut self.handleSR;
            match handle {
                Some(hand) => {
                    let mem_delta = hand.add(&self.key[..], self.cols, rs, Some(self.uid));
                    if mem_delta > 0 {
                        self.mem_size += mem_delta as usize;
                    } else if mem_delta < 0 {
                        self.mem_size = self
                            .mem_size
                            .checked_sub(mem_delta.checked_abs().unwrap() as usize)
                            .unwrap();
                    }
                },
                None => {},
            }
        } else {
            let handle = &mut self.handle
            ;
            match handle {
                Some(hand) => {
                    let mem_delta = hand.add(&self.key[..], self.cols, rs);
                    if mem_delta > 0 {
                        self.mem_size += mem_delta as usize;
                    } else if mem_delta < 0 {
                        self.mem_size = self
                            .mem_size
                            .checked_sub(mem_delta.checked_abs().unwrap() as usize)
                            .unwrap();
                    }
                },
                None => {},
            }
        }
    }

    pub(crate) fn is_partial(&self) -> bool {
        self.partial
    }

    /// Evict `count` randomly selected keys from state and return them along with the number of
    /// bytes that will be freed once the underlying `evmap` applies the operation.
    pub fn evict_random_key(&mut self, rng: &mut ThreadRng) -> u64 {
        if self.srmap {
            let handle = &mut self.handleSR;
            match handle {
                Some(hand) => {
                    let mut bytes_to_be_freed = 0;
                    if self.mem_size > 0 {
                        if hand.is_empty() {
                            unreachable!("mem size is {}, but map is empty", self.mem_size);
                        }

                        match hand.empty_at_index(rng.gen()) {
                            None => (),
                            Some(vs) => {
                                let size: u64 = vs.into_iter().map(|r| r.deep_size_of() as u64).sum();
                                bytes_to_be_freed += size;
                            }
                        }
                        self.mem_size = self
                            .mem_size
                            .checked_sub(bytes_to_be_freed as usize)
                            .unwrap();
                    }
                    bytes_to_be_freed

                },
                None => {0},
            }
        } else {
            let handle = &mut self.handle;
            match handle {
                Some(hand) => {
                    let mut bytes_to_be_freed = 0;
                    if self.mem_size > 0 {
                        if hand.is_empty() {
                            unreachable!("mem size is {}, but map is empty", self.mem_size);
                        }

                        match hand.empty_at_index(rng.gen()) {
                            None => (),
                            Some(vs) => {
                                let size: u64 = vs.into_iter().map(|r| r.deep_size_of() as u64).sum();
                                bytes_to_be_freed += size;
                            }
                        }
                        self.mem_size = self
                            .mem_size
                            .checked_sub(bytes_to_be_freed as usize)
                            .unwrap();
                    }
                    bytes_to_be_freed

                },
                None => {0},
            }

        }
    }
}

impl SizeOf for WriteHandle {
    fn size_of(&self) -> u64 {
        use std::mem::size_of;

        size_of::<Self>() as u64
    }

    fn deep_size_of(&self) -> u64 {
        self.mem_size as u64
    }
}

/// Handle to get the state of a single shard of a reader.
#[derive(Clone)]
pub struct SingleReadHandle {
    handle: Option<multir::Handle>,
    handleSR: Option<multir_sr::Handle>,
    srmap: bool,
    trigger: Option<Arc<Fn(&[DataType], Option<usize>) + Send + Sync>>,
    key: Vec<usize>,
    pub uid: usize,
}

impl SingleReadHandle {
    pub fn clone_new_user(&mut self, r: multir_sr::Handle, uid: usize) -> SingleReadHandle {
        SingleReadHandle {
           handle: None,
           handleSR: Some(r),
           srmap: true,
           trigger: self.trigger.clone(),
           key: self.key.clone(),
           uid: uid.clone(),
       }
    }

    pub fn clone_new_user_partial(&mut self, r: multir_sr::Handle, uid: usize, trigger: Option<Arc<Fn(&[DataType], Option<usize>) + Send + Sync>>) -> SingleReadHandle {
        SingleReadHandle {
           handle: None,
           handleSR: Some(r),
           srmap: true,
           trigger: trigger,
           key: self.key.clone(),
           uid: uid.clone(),
       }
    }

    pub fn clone(&mut self, r: multir_sr::Handle, uid: usize) -> SingleReadHandle {
        SingleReadHandle {
           handle: None,
           handleSR: Some(r),
           srmap: true,
           trigger: self.trigger.clone(),
           key: self.key.clone(),
           uid: uid.clone(),
       }
    }

    pub fn universe(&self) -> usize{
       self.uid.clone()
    }

    /// Trigger a replay of a missing key from a partially materialized view.
    pub fn trigger(&self, key: &[DataType], id: Option<usize>) {
        println!("triggering, uid: {:?}", id);
        assert!(
            self.trigger.is_some(),
            "tried to trigger a replay for a fully materialized view"
        );

        // trigger a replay to populate
        (*self.trigger.as_ref().unwrap())(key, id);
    }

    /// Find all entries that matched the given conditions.
    ///
    /// Returned records are passed to `then` before being returned.
    ///
    /// Note that not all writes will be included with this read -- only those that have been
    /// swapped in by the writer.
    ///
    /// Holes in partially materialized state are returned as `Ok((None, _))`.
    pub fn try_find_and<F, T>(&mut self, key: &[DataType], mut then: F) -> Result<(Option<T>, i64), ()>
    where
        F: FnMut(&[Vec<DataType>]) -> T,
    {
        if self.srmap {
            println!("try find and. uid: {:?}", self.uid);
            let handle = &mut self.handleSR;
            match handle {
                Some(hand) => {
                    hand
                    .meta_get_and(key, &mut then)
                    .ok_or(())
                    .map(|(mut records, meta)| {
                        if records.is_none() && self.trigger.is_none() {
                            records = Some(then(&[]));
                        }
                        (records, meta)
                    })
                },
                None => {Err(())},
            }
        } else {
            let handle = &mut self.handle;
            match handle {
                Some(hand) => {
                    hand
                    .meta_get_and(key, &mut then)
                    .ok_or(())
                    .map(|(mut records, meta)| {
                        if records.is_none() && self.trigger.is_none() {
                            records = Some(then(&[]));
                        }
                        (records, meta)
                    })
                },
                None => {Err(())},
            }
        }
    }

    #[allow(dead_code)]
    pub fn len(&mut self) -> usize {
        if self.srmap {
            let handle = &mut self.handleSR;
            match handle {
                Some(hand) => {
                    hand.len()
                },
                None => { 0 }
            }
        } else {
            let handle = &mut self.handle;
            match handle {
                Some(hand) => {
                    hand.len()
                },
                None => { 0 }
            }
        }
    }

    /// Count the number of rows in the reader.
    /// This is a potentially very costly operation, since it will
    /// hold up writers until all rows are iterated through.
    pub fn count_rows(&self) -> usize {
        let mut nrows = 0;
        if self.srmap {
            let handle = &self.handleSR;
            match handle {
                Some(hand) => {
                    hand.for_each(|v| nrows += v.len());
                    nrows
                },
                None => {0},
            }
        } else {
            let handle = &self.handle;
            match handle {
                Some(hand) => {
                    hand.for_each(|v| nrows += v.len());
                    nrows
                },
                None => {0},
            }
        }
    }
}

#[derive(Clone)]
pub enum ReadHandle {
    Sharded(Vec<Option<SingleReadHandle>>),
    Singleton(Option<SingleReadHandle>),
}

impl ReadHandle {
    /// Find all entries that matched the given conditions.
    ///
    /// Returned records are passed to `then` before being returned.
    ///
    /// Note that not all writes will be included with this read -- only those that have been
    /// swapped in by the writer.
    ///
    /// A hole in partially materialized state is returned as `Ok((None, _))`.
    pub fn try_find_and<F, T>(&mut self, key: &[DataType], then: F) -> Result<(Option<T>, i64), ()>
    where
        F: FnMut(&[Vec<DataType>]) -> T,
    {

        match *self {
            // ReadHandle::Sharded(ref mut shards) => {
            //     assert_eq!(key.len(), 1);
            //     match shards[::shard_by(&key[0], shards.len())] {
            //         Some(ref mut inner) => {
            //             inner.try_find_and(key, then)
            //         },
            //         None => {panic!("shouldn't happen")}
            //     }
            // }
            ReadHandle::Singleton(ref mut srh) => {
                match srh {
                    Some(inner) => {
                        let res = inner.try_find_and(key, then);
                        res
                    }
                    _ => panic!("unimplemented"),
                }
            },
            _ => panic!("can't get this to compile")
        }
    }

    pub fn len(&mut self) -> usize {
        match *self {
            // ReadHandle::Sharded(ref shards) => {
            //     shards.iter().map(|s| s.as_ref().unwrap().len()).sum()
            // }
            ReadHandle::Singleton(ref mut
                srh) => {
                match srh {
                    Some(ref mut
                        inner) => inner.len(),
                    None => panic!("unimplemented"),
                }
            },
            _ => panic!("couldn't get this to compile"),
        }
    }

    pub fn set_single_handle(&mut self, shard: Option<usize>, handle: SingleReadHandle) {
        match (self, shard) {
            (&mut ReadHandle::Singleton(ref mut srh), None) => {
                *srh = Some(handle);
            }
            (&mut ReadHandle::Sharded(ref mut rhs), None) => {
                // when ::SHARDS == 1, sharded domains think they're unsharded
                assert_eq!(rhs.len(), 1);
                let srh = rhs.get_mut(0).unwrap();
                assert!(srh.is_none());
                *srh = Some(handle)
            }
            (&mut ReadHandle::Sharded(ref mut rhs), Some(shard)) => {
                let srh = rhs.get_mut(shard).unwrap();
                assert!(srh.is_none());
                *srh = Some(handle)
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_works() {
        let a = vec![1.into(), "a".into()];

        let (r, mut w) = new(2, &[0]);

        // initially, store is uninitialized
        assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()), Err(()));

        w.swap();

        // after first swap, it is empty, but ready
        assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()), Ok((Some(0), -1)));

        w.add(vec![Record::Positive(a.clone())]);

        // it is empty even after an add (we haven't swapped yet)
        assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()), Ok((Some(0), -1)));

        w.swap();

        // but after the swap, the record is there!
        assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
        assert!(
            r.try_find_and(&a[0..1], |rs| rs
                .iter()
                .any(|r| r[0] == a[0] && r[1] == a[1])).unwrap()
            .0
            .unwrap()
        );
    }

    #[test]
    fn busybusybusy() {
        use std::thread;

        let n = 10000;
        let (r, mut w) = new(1, &[0]);
        thread::spawn(move || {
            for i in 0..n {
                w.add(vec![Record::Positive(vec![i.into()])]);
                w.swap();
            }
        });

        for i in 0..n {
            let i = &[i.into()];
            loop {
                match r.try_find_and(i, |rs| rs.len()) {
                    Ok((None, _)) => continue,
                    Ok((Some(1), _)) => break,
                    Ok((Some(i), _)) => assert_ne!(i, 1),
                    Err(()) => continue,
                }
            }
        }
    }

    #[test]
    fn minimal_query() {
        let a = vec![1.into(), "a".into()];
        let b = vec![1.into(), "b".into()];

        let (r, mut w) = new(2, &[0]);
        w.add(vec![Record::Positive(a.clone())]);
        w.swap();
        w.add(vec![Record::Positive(b.clone())]);

        assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
        assert!(
            r.try_find_and(&a[0..1], |rs| rs
                .iter()
                .any(|r| r[0] == a[0] && r[1] == a[1])).unwrap()
            .0
            .unwrap()
        );
    }

    #[test]
    fn non_minimal_query() {
        let a = vec![1.into(), "a".into()];
        let b = vec![1.into(), "b".into()];
        let c = vec![1.into(), "c".into()];

        let (r, mut w) = new(2, &[0]);
        w.add(vec![Record::Positive(a.clone())]);
        w.add(vec![Record::Positive(b.clone())]);
        w.swap();
        w.add(vec![Record::Positive(c.clone())]);

        assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(2));
        assert!(
            r.try_find_and(&a[0..1], |rs| rs
                .iter()
                .any(|r| r[0] == a[0] && r[1] == a[1])).unwrap()
            .0
            .unwrap()
        );
        assert!(
            r.try_find_and(&a[0..1], |rs| rs
                .iter()
                .any(|r| r[0] == b[0] && r[1] == b[1])).unwrap()
            .0
            .unwrap()
        );
    }

    #[test]
    fn absorb_negative_immediate() {
        let a = vec![1.into(), "a".into()];
        let b = vec![1.into(), "b".into()];

        let (r, mut w) = new(2, &[0]);
        w.add(vec![Record::Positive(a.clone())]);
        w.add(vec![Record::Positive(b.clone())]);
        w.add(vec![Record::Negative(a.clone())]);
        w.swap();

        assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
        assert!(
            r.try_find_and(&a[0..1], |rs| rs
                .iter()
                .any(|r| r[0] == b[0] && r[1] == b[1])).unwrap()
            .0
            .unwrap()
        );
    }

    #[test]
    fn srmap_works() {
        let a = vec![1.into(), "a".into()];
        let b = vec![1.into(), "b".into()];
        let a_rec = vec![Record::Positive(a.clone())];
        let b_rec = vec![Record::Positive(b.clone())];

        let (mut r1, mut w1) = new(true, 2, &[0], 0);
        let (mut r2, mut w2) = w1.clone_new_user(r1.clone());
        let (mut r3, mut w3) = w1.clone_new_user(r1.clone());

        w1.add(a_rec.clone());
        w2.add(a_rec.clone());
        w2.add(b_rec.clone());
        w3.add(a_rec.clone());


        r1.try_find_and(&a[0..1], |rs| println!("Rs: {:?}", rs.clone()));
        r2.try_find_and(&a[0..1], |rs| println!("Rs: {:?}", rs.clone()));
        r3.try_find_and(&a[0..1], |rs| println!("Rs: {:?}", rs.clone()));
        r3.try_find_and(&b[0..1], |rs| println!("Rs: {:?}", rs.clone()));
        r2.try_find_and(&b[0..1], |rs| println!("Rs: {:?}", rs.clone()));

        // assert_eq!(r3.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
        // assert_eq!(r2.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
        // assert_eq!(r2.try_find_and(&b[0..1], |rs| rs.len()).unwrap().0, Some(1));
        // assert_eq!(r3.try_find_and(&b[0..1], |rs| rs.len()).unwrap().0, Some(0));

    }

    #[test]
    fn absorb_negative_later() {
        let a = vec![1.into(), "a".into()];
        let b = vec![1.into(), "b".into()];

        let (r, mut w) = new(2, &[0]);
        w.add(vec![Record::Positive(a.clone())]);
        w.add(vec![Record::Positive(b.clone())]);
        w.swap();
        w.add(vec![Record::Negative(a.clone())]);
        w.swap();

        assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
        assert!(
            r.try_find_and(&a[0..1], |rs| rs
                .iter()
                .any(|r| r[0] == b[0] && r[1] == b[1])).unwrap()
            .0
            .unwrap()
        );
    }

    #[test]
    fn absorb_multi() {
        let a = vec![1.into(), "a".into()];
        let b = vec![1.into(), "b".into()];
        let c = vec![1.into(), "c".into()];

        let (r, mut w) = new(2, &[0]);
        w.add(vec![
            Record::Positive(a.clone()),
            Record::Positive(b.clone()),
        ]);
        w.swap();

        assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(2));
        assert!(
            r.try_find_and(&a[0..1], |rs| rs
                .iter()
                .any(|r| r[0] == a[0] && r[1] == a[1])).unwrap()
            .0
            .unwrap()
        );
        assert!(
            r.try_find_and(&a[0..1], |rs| rs
                .iter()
                .any(|r| r[0] == b[0] && r[1] == b[1])).unwrap()
            .0
            .unwrap()
        );

        w.add(vec![
            Record::Negative(a.clone()),
            Record::Positive(c.clone()),
            Record::Negative(c.clone()),
        ]);
        w.swap();

        assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
        assert!(
            r.try_find_and(&a[0..1], |rs| rs
                .iter()
                .any(|r| r[0] == b[0] && r[1] == b[1])).unwrap()
            .0
            .unwrap()
        );
    }
}
