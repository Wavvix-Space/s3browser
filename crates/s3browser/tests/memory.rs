//! 量一下装满条目之后浏览状态占多少堆内存。
//!
//! 设计文档第 10 节给十万条目的目录定的预算是额外不超过 15 MB。整个进程的
//! WorkingSet 不好在测试里稳定测出来（GPUI、显卡驱动、系统缓存都算在里面），
//! 但"条目本身占多少"是可以测准的，而且这才是我们能控制的部分。
//!
//! 单独放一个测试文件，是因为这里装了全局分配器，不想影响别的测试。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use s3api::ObjectMeta;
use s3browser::browser_state::{BrowserState, Location, ViewMode};

static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// 计数器是整个进程共用的，两条用例并行跑会互相算进对方的账上。
/// 测试默认是多线程跑的，所以自己串起来。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// 造一批名字长度接近真实的对象。key 里带目录前缀，条目只该留文件名那一段。
fn page(prefix: &str, from: usize, count: usize) -> Vec<ObjectMeta> {
    (from..from + count)
        .map(|i| ObjectMeta {
            key: format!("{prefix}IMG_{i:07}_20260920_backup.jpg"),
            size: 1024 * (i as u64 % 4096),
            modified: 1_760_000_000 + i as i64,
            etag: None,
            storage_class: None,
            content_type: None,
        })
        .collect()
}

#[test]
fn a_hundred_thousand_entries_fit_the_budget() {
    const COUNT: usize = 100_000;
    const BUDGET: usize = 15 * 1024 * 1024;
    let prefix = "photos/2026/09/";
    let _guard = serial();

    let mut state = BrowserState::new(ViewMode::List, true);
    // 位置要和 key 的前缀对上：ListObjectsV2 带 delimiter 返回的就是当前目录下的 key，
    // 条目只留最后那一段文件名
    state.navigate_to(
        Location::root("p1", "bucket")
            .child("photos")
            .child("2026")
            .child("09"),
    );

    let before = live();

    // 分页灌进去，和真实翻页一样；每页的原始数据用完就扔，
    // 留下的才是 BrowserState 自己持有的
    let mut token = Some(String::new());
    for chunk in 0..(COUNT / 1000) {
        let objects = page(prefix, chunk * 1000, 1000);
        if chunk == 0 {
            state.set_page(&objects, &[], token.take());
        } else {
            state.append_page(&objects, &[], None);
        }
    }

    let used = live().saturating_sub(before);
    let per_entry = used as f64 / COUNT as f64;

    assert_eq!(state.entries().len(), COUNT);
    // 条目只存文件名，不存整条 key
    assert_eq!(
        state.entry(0).unwrap().name.as_ref(),
        "IMG_0000000_20260920_backup.jpg"
    );

    eprintln!(
        "十万条目占用 {:.1} MB，平均每条 {per_entry:.0} 字节",
        used as f64 / 1048576.0
    );
    assert!(used < BUDGET, "十万条目用了 {used} 字节，超过预算 {BUDGET}");
}

/// 离开目录之后这些内存要还回去，不能攒着。
#[test]
fn leaving_a_directory_releases_the_entries() {
    let _guard = serial();
    let mut state = BrowserState::new(ViewMode::List, true);
    state.navigate_to(Location::root("p1", "bucket").child("d"));

    let before = live();
    let objects = page("d/", 0, 50_000);
    state.set_page(&objects, &[], None);
    drop(objects);
    let loaded = live();
    assert!(loaded > before, "灌进去之后应该涨");

    // 换个目录，旧列表就该释放
    state.navigate_to(Location::root("p1", "other"));
    let after = live();

    assert!(
        after < before + (loaded - before) / 10,
        "换目录之后没释放：之前 {before}，装满 {loaded}，现在 {after}"
    );
}
