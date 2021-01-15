// For the most part, shamelessly copied from carllerche futures mpsc tests
extern crate futures;
extern crate multiqueue2 as multiqueue;

use futures::executor::block_on;
use futures::future::{lazy, Future};
use futures::sink::SinkExt;
use futures::stream::StreamExt;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

fn is_send<T: Send>() {}

#[test]
fn bounds() {
    is_send::<multiqueue::BroadcastFutSender<i32>>();
    is_send::<multiqueue::BroadcastFutReceiver<i32>>();
}

#[test]
fn send_recv() {
    block_on(async {
        let (tx, mut rx) = multiqueue::broadcast_fut_queue::<i32>(16);
    
        tx.send(1).await.unwrap();
    
        assert_eq!(rx.next().await.unwrap(), Ok(1));    
    })
}

#[test]
fn send_shared_recv() {
    block_on(async {
        let (mut tx1, mut rx) = multiqueue::broadcast_fut_queue::<i32>(16);
        let mut tx2 = tx1.clone();
    
        tx1.send(1).await.unwrap();
        assert_eq!(rx.next().await.unwrap(), Ok(1));
    
        tx2.send(2).await.unwrap();
        assert_eq!(rx.next().await.unwrap(), Ok(2));    
    })
}

#[test]
fn send_recv_threads() {
    block_on(async {
        let (tx, rx) = multiqueue::broadcast_fut_queue::<i32>(16);
        let mut rx = rx.wait();
    
        thread::spawn(move || {
            tx.send(1).wait().unwrap();
        });
    
        assert_eq!(rx.next().unwrap(), Ok(1));    
    })
}

#[test]
fn send_recv_threads_no_capacity() {
    let (mut tx, rx) = multiqueue::broadcast_fut_queue::<i32>(0);
    let mut rx = rx.wait();

    let t = thread::spawn(move || {
        tx = tx.send(1).wait().unwrap();
        tx.send(2).wait().unwrap();
    });

    thread::sleep(Duration::from_millis(100));
    assert_eq!(rx.next().unwrap(), Ok(1));

    thread::sleep(Duration::from_millis(100));
    assert_eq!(rx.next().unwrap(), Ok(2));

    t.join().unwrap();
}

#[test]
fn recv_close_gets_none() {
    let (tx, rx) = multiqueue::broadcast_fut_queue::<i32>(10);

    // Run on a task context
    lazy(move || {
        rx.unsubscribe();

        drop(tx);

        Ok::<(), ()>(())
    })
    .wait()
    .unwrap();
}

#[test]
fn tx_close_gets_none() {
    let (_, mut rx) = multiqueue::broadcast_fut_queue::<i32>(10);

    // Run on a task context
    lazy(move || {
        assert_eq!(rx.poll(), Ok(Async::Ready(None)));
        assert_eq!(rx.poll(), Ok(Async::Ready(None)));

        Ok::<(), ()>(())
    })
    .wait()
    .unwrap();
}

#[test]
fn stress_shared_bounded_hard() {
    const AMT: u32 = 10000;
    const NTHREADS: u32 = 8;
    let (tx, rx) = multiqueue::broadcast_fut_queue::<i32>(0);
    let mut rx = rx.wait();

    let t = thread::spawn(move || {
        for _ in 0..AMT * NTHREADS {
            assert_eq!(rx.next().unwrap(), Ok(1));
        }

        if rx.next().is_some() {
            panic!();
        }
    });

    for _ in 0..NTHREADS {
        let mut tx = tx.clone();

        thread::spawn(move || {
            for _ in 0..AMT {
                tx = tx.send(1).wait().unwrap();
            }
        });
    }

    drop(tx);

    t.join().ok().unwrap();
}

#[test]
fn stress_receiver_multi_task_bounded_hard() {
    const AMT: usize = 10_000;
    const NTHREADS: u32 = 2;

    let (mut tx, rx) = multiqueue::broadcast_fut_queue::<usize>(0);
    let rx = Arc::new(Mutex::new(Some(rx)));
    let n = Arc::new(AtomicUsize::new(0));

    let mut th = vec![];

    for _ in 0..NTHREADS {
        let rx = rx.clone();
        let n = n.clone();

        let t = thread::spawn(move || {
            let mut i = 0;

            loop {
                i += 1;
                let mut lock = rx.lock().ok().unwrap();

                match lock.take() {
                    Some(mut rx) => {
                        if i % 5 == 0 {
                            let (item, rest) = rx.into_future().wait().ok().unwrap();

                            if item.is_none() {
                                break;
                            }

                            n.fetch_add(1, Ordering::Relaxed);
                            *lock = Some(rest);
                        } else {
                            // Just poll
                            let n = n.clone();
                            let r = lazy(move || {
                                let r = match rx.poll().unwrap() {
                                    Async::Ready(Some(_)) => {
                                        n.fetch_add(1, Ordering::Relaxed);
                                        *lock = Some(rx);
                                        false
                                    }
                                    Async::Ready(None) => true,
                                    Async::NotReady => {
                                        *lock = Some(rx);
                                        false
                                    }
                                };

                                Ok::<bool, ()>(r)
                            })
                            .wait()
                            .unwrap();

                            if r {
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
        });

        th.push(t);
    }

    for i in 0..AMT {
        tx = tx.send(i).wait().unwrap();
    }

    drop(tx);

    for t in th {
        t.join().unwrap();
    }

    assert_eq!(AMT, n.load(Ordering::Relaxed));
}
