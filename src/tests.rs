/*!
This test framework intends to combine both performance and correctness tests
into a single test.

It will do this by measuring the performance of ONLY the allocation and deallocation,
not measure it's own checking

But performance checking will be secondary, the primary goal of this suite is
correctness testing.

It will have:
 - pseudo random number generator which ensures that allocations are identical
     on each run (can be altered by changing seed)
 - "Allocation Array" which tracks and determines which allocations shall be
      made.
*/


use std::string::String;
use std::vec::Vec;
use std::panic;

use core::mem;
use core::iter::FromIterator;
use core::result;

use test::Bencher;
use rand::{sample, Rng, SeedableRng, XorShiftRng};
use stopwatch::Stopwatch;

use super::*;
use super::types::{BlockLoc, IndexLoc};

type Fill = u32;
type TResult<T> = result::Result<T, String>;

impl panic::UnwindSafe for Pool {}


#[derive(Debug, Default)]
struct Stats {
    loops: usize,
    allocs: usize,
    allocs_fast: usize,
    alloc_skips: usize,
    frees: usize,
    cleans: usize,
    defrags: usize,
    out_of_mems: usize,
}

/// Actions that can be done when a full allocation is found
#[derive(Debug, Copy, Clone)]
enum FullActions {
    Deallocate,
    Clean,
    Change,
}

/// Actions that can be done when an empty allocation is found
#[derive(Debug, Copy, Clone)]
enum EmptyActions {
    Alloc,
    AllocFast,
    Skip,
}

#[derive(Debug, Default, Clone)]
struct Settings {
    /// the number of loops to run
    loops: usize,
    /// an action will randomly be selected from this list when
    /// data is found in an allocation
    full_chances: Vec<FullActions>,
    /// an action will randomly be selected from this list when
    /// data is not found in an allocation
    empty_chances: Vec<EmptyActions>,
}

/// contains means to track test as well as
/// settings for test
struct Tracker {
    gen: XorShiftRng,
    clock: Stopwatch,
    test_clock: Stopwatch,
    stats: Stats,
    settings: Settings,
}

impl Tracker {
    pub fn new(settings: Settings) -> Tracker {
        let seed = [1, 2, 3, 4];
        let gen = XorShiftRng::from_seed(seed);
        Tracker {
            gen: gen,
            clock: Stopwatch::new(),
            test_clock: Stopwatch::new(),
            stats: Stats::default(),
            settings: settings,
        }
    }
}

struct Allocation<'a> {
    pool: &'a Pool,
    data: Vec<Fill>,
    mutex: Option<super::SliceMutex<'a, Fill>>,
}

impl<'a> Allocation<'a> {
    fn assert_valid(&mut self) -> TResult<()> {
        let mutex = match self.mutex {
            Some(ref mut m) => m,
            None => return Ok(()),
        };
        let ref sdata = self.data;
        let pdata = mutex.lock();

        if sdata.len() != pdata.len() {
            return Err(format!("lengths not equal: {} != {}", sdata.len(), pdata.len()));
        }
        for (i, (s, p)) in sdata.iter().zip(pdata.iter()).enumerate() {
            if s != p {
                return Err(format!("values at i={} differ: {} != {}", i, s, p));
            }
        }
        Ok(())
    }

    /// fill the Allocation up with data, don't check
    fn fill(&mut self, t: &mut Tracker) -> TResult<()> {
        let mutex = match self.mutex {
            Some(ref mut m) => m,
            None => return Ok(()),
        };
        let edata = &mut self.data;
        let mut pdata = mutex.lock();
        for (e, p) in edata.iter_mut().zip(pdata.iter_mut()) {
            let val = t.gen.gen::<Fill>();
            *e = val;
            *p = val;
        }
        Ok(())
    }

    /// allocate some new data and fill it
    fn alloc(&mut self, t: &mut Tracker, fast: bool) -> TResult<()> {
        assert!(self.mutex.is_none());
        let divider = self.pool.size() / (mem::size_of::<Fill>() * 64);
        let len = t.gen.gen::<u16>() % divider as u16;
        t.clock.start();
        let slice = if fast {
            self.pool.alloc_slice_fast::<Fill>(len)
        } else {
            self.pool.alloc_slice::<Fill>(len)
        };
        t.clock.stop();
        self.mutex =
            Some(match slice {
                     Ok(m) => {
                         t.stats.allocs += 1;
                         m
                     }
                     Err(e) => {
                         match e {
                             Error::OutOfMemory => {
                                 t.stats.out_of_mems += 1;
                                 return Ok(()); // not allocated
                             }
                             Error::Fragmented => {
                                 // auto-defrag whenever it is encountered
                                 t.clock.start();
                                 self.pool.defrag();
                                 let slice = self.pool.alloc_slice::<Fill>(len);
                                 t.clock.stop();
                                 t.stats.defrags += 1;
                                 match slice {
                                     Ok(m) => {
                                         t.stats.allocs += 1;
                                         m
                                     }
                                     Err(e) => return Err(format!("alloc::alloc_slice2: {:?}", e)),
                                 }
                             }
                             _ => return Err(format!("alloc::aloc_slice:{:?}", e)),
                         }
                     }
                 });
        self.data.clear();
        for _ in 0..len {
            self.data.push(0);
        }
        try!(self.fill(t));
        Ok(())
    }

    /// do something randomly
    fn do_random(&mut self, t: &mut Tracker) -> TResult<()> {
        try!(self.assert_valid());
        match self.mutex {
            // we have data, we need to decide what to do with it
            Some(_) => {
                match sample(&mut t.gen, &t.settings.full_chances, 1)[0] {
                    &FullActions::Deallocate => {
                        // deallocate the data
                        self.mutex = None;
                        t.stats.frees += 1;
                    }
                    &FullActions::Clean => {
                        // clean the data
                        t.clock.start();
                        self.pool.clean();
                        t.clock.stop();
                        t.stats.cleans += 1;
                    }
                    &FullActions::Change => {
                        // change the data
                        try!(self.fill(t));
                    }
                }
            }
            // there is no data, should we allocate it?
            None => {
                match sample(&mut t.gen, &t.settings.empty_chances, 1)[0] {
                    &EmptyActions::Alloc => try!(self.alloc(t, false)),
                    &EmptyActions::AllocFast => try!(self.alloc(t, true)),
                    &EmptyActions::Skip => t.stats.alloc_skips += 1,
                }
            }
        }
        try!(self.assert_valid());
        Ok(())
    }
}


// TODO: several parameters (like number of loops) need to be moved into settings
// and then several "benchmark" tests need to be created that can only be run in release
// mode... in release 1000 loops takes < 1 sec, in debug mode it takes over a minute.
fn do_test(allocs: &mut Vec<Allocation>, track: &mut Tracker) {
    println!("len allocs: {}", allocs.len());
    println!("some random values: {}, {}, {}",
             track.gen.gen::<u16>(),
             track.gen.gen::<u16>(),
             track.gen.gen::<u16>());
    track.test_clock.start();
    for _ in 0..track.settings.loops {
        for alloc in allocs.iter_mut() {
            alloc.do_random(track).unwrap();
        }
        track.stats.loops += 1;
    }
    track.test_clock.stop();
}

fn run_test(name: &str,
            settings: Settings,
            blocks: BlockLoc,
            indexes: IndexLoc,
            index_cache: IndexLoc) {
    let mut track = Tracker::new(settings);

    let size = blocks as usize * mem::size_of::<Block>();
    let pool = Pool::new(size, indexes, index_cache).expect("can't get pool");
    let mut allocs = Vec::from_iter((0..pool.len_indexes()).map(|_| {
                                                                    Allocation {
                                                                        pool: &pool,
                                                                        data: Vec::new(),
                                                                        mutex: None,
                                                                    }
                                                                }));

    let res = panic::catch_unwind(panic::AssertUnwindSafe(|| do_test(&mut allocs, &mut track)));
    println!("## {}", name);
    match res {
        Ok(_) => {}
        Err(_) => {
            println!("{}", pool.display());
        }
    }
    println!("TIMES: test={}ms, pool={}ms",
             track.test_clock.elapsed_ms(),
             track.clock.elapsed_ms());
    println!("STATS: {:?}", track.stats);
    match res {
        Ok(_) => {}
        Err(e) => {
            panic::resume_unwind(e);
        }
    };
}

pub const BLOCKS: BlockLoc = u16::max_value() / 2;
// pub const INDEXES: IndexLoc = BLOCKS / 128;
pub const INDEXES: IndexLoc = 512;
pub const LOOPS: usize = 1000;

#[test]
fn small_integration() {
    let mut settings = Settings {
        loops: 50,
        full_chances: Vec::from_iter([FullActions::Deallocate; 9].iter().cloned()),
        empty_chances: vec![EmptyActions::Alloc],
    };
    settings.full_chances.push(FullActions::Clean);
    settings.full_chances.push(FullActions::Change);
    settings.empty_chances.push(EmptyActions::Skip);
    run_test("small_integration", settings, BLOCKS, INDEXES, INDEXES / 10);
}

#[bench]
fn bench_no_cache(_: &mut Bencher) {
    let mut settings = Settings {
        loops: LOOPS,
        full_chances: Vec::from_iter([FullActions::Deallocate; 9].iter().cloned()),
        empty_chances: vec![EmptyActions::Alloc],
    };
    settings.full_chances.push(FullActions::Clean);
    run_test("bench_no_cache", settings.clone(), BLOCKS, INDEXES, 1);
}

#[bench]
fn bench_large_cache(_: &mut Bencher) {
    let mut settings = Settings {
        loops: LOOPS,
        full_chances: Vec::from_iter([FullActions::Deallocate; 9].iter().cloned()),
        empty_chances: vec![EmptyActions::Alloc],
    };
    settings.full_chances.push(FullActions::Clean);
    run_test("bench_large_cache",
             settings.clone(),
             BLOCKS,
             INDEXES,
             INDEXES);
}

#[bench]
fn bench_small_cache(_: &mut Bencher) {
    let mut settings = Settings {
        loops: LOOPS,
        full_chances: Vec::from_iter([FullActions::Deallocate; 9].iter().cloned()),
        empty_chances: vec![EmptyActions::Alloc],
    };
    settings.full_chances.push(FullActions::Clean);
    run_test("bench_small_cache",
             settings.clone(),
             BLOCKS,
             INDEXES,
             INDEXES / 20);
}

#[bench]
fn bench_fast_large_cache(_: &mut Bencher) {

    let mut settings = Settings {
        loops: LOOPS,
        full_chances: Vec::from_iter([FullActions::Deallocate; 9].iter().cloned()),
        empty_chances: vec![EmptyActions::AllocFast],
    };
    settings.full_chances.push(FullActions::Clean);
    run_test("bench_fast_large_cache",
             settings.clone(),
             BLOCKS,
             INDEXES,
             INDEXES);
}
