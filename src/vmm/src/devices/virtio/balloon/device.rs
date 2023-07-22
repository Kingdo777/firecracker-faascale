// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::cmp;
use std::io::Write;
use std::result::Result;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use logger::{error, IncMetric, METRICS};
use serde::Serialize;
use timerfd::{ClockId, SetTimeFlags, TimerFd, TimerState};
use utils::eventfd::EventFd;
use utils::vm_memory::{Address, ByteValued, Bytes, GuestAddress, GuestMemoryMmap};
use virtio_gen::virtio_blk::VIRTIO_F_VERSION_1;

use super::super::{ActivateResult, DeviceState, Queue, VirtioDevice, TYPE_BALLOON};
use super::util::{compact_page_frame_numbers, remove_range};
use super::{
    BALLOON_DEV_ID, DEFLATE_INDEX, INFLATE_INDEX, MAX_PAGES_IN_DESC, MAX_PAGE_COMPACT_BUFFER,
    MIB_TO_4K_PAGES, NUM_QUEUES, QUEUE_SIZES, STATS_INDEX, VIRTIO_BALLOON_F_DEFLATE_ON_OOM,
    VIRTIO_BALLOON_F_STATS_VQ, VIRTIO_BALLOON_PFN_SHIFT, VIRTIO_BALLOON_S_AVAIL,
    VIRTIO_BALLOON_S_CACHES, VIRTIO_BALLOON_S_HTLB_PGALLOC, VIRTIO_BALLOON_S_HTLB_PGFAIL,
    VIRTIO_BALLOON_S_MAJFLT, VIRTIO_BALLOON_S_MEMFREE, VIRTIO_BALLOON_S_MEMTOT,
    VIRTIO_BALLOON_S_MINFLT, VIRTIO_BALLOON_S_SWAP_IN, VIRTIO_BALLOON_S_SWAP_OUT,
};
use crate::devices::virtio::balloon::Error as BalloonError;
use crate::devices::virtio::{IrqTrigger, IrqType};

/// SIZE_OF_U32和SIZE_OF_STAT，分别表示u32和BalloonStat类型的大小（以字节为单位）
const SIZE_OF_U32: usize = std::mem::size_of::<u32>();
/// std::mem::size_of函数来获取类型的大小
const SIZE_OF_STAT: usize = std::mem::size_of::<BalloonStat>();

/// 将以MB为单位的数量转换为以4KB页面为单位的数量, 如果乘法不会导致溢出，返回乘法结果，否则返回一个溢出错误
fn mib_to_pages(amount_mib: u32) -> Result<u32, BalloonError> {
    amount_mib
        .checked_mul(MIB_TO_4K_PAGES)
        .ok_or(BalloonError::TooManyPagesRequested)
}

/// 将以4KB页面为单位的数量转换为以MB为单位的数量
fn pages_to_mib(amount_pages: u32) -> u32 {
    amount_pages / MIB_TO_4K_PAGES
}

#[repr(C)] /// #[repr(C)] 表示按照 C 语言的内存布局方式对结构体进行排列
/// 这是 Rust 中的一个派生宏（derive macro）的示例，这个宏会自动为一个结构体或者枚举类型实现一些常用的 trait 方法。
/// 具体来说，这个宏实现了 Clone、Copy、Debug、Default 和 PartialEq 这几个 trait。其中：
///     1. Clone trait 表示这个类型可以通过复制本身来创建一个新的对象，这个新对象与原对象是独立的。
///     2. Copy trait 表示这个类型可以通过直接复制内存来创建一个新的对象，这个新对象与原对象也是独立的。需要注意的是，Copy trait 只适用于简单的、内存连续的数据类型，如整数、浮点数、布尔值、指针、元组等等。
///     3. Debug trait 表示这个类型可以通过调试输出的方式展示自己。
///     4. Default trait 表示这个类型可以通过默认值来创建一个新的对象。
///     5. PartialEq trait 表示这个类型可以进行等值比较操作。
/// 通过使用派生宏可以减少开发者的工作量，简化代码实现过程，同时也可以避免一些常见的错误。需要注意的是，派生宏需要应用在符合某些限制的结构体或枚举上，
/// 这些限制包括类型必须是 Plain Old Data（POD）类型、不能包含泛型参数等等。如果遇到不符合限制的情况，编译器会产生相应的错误提示
#[derive(Clone, Copy, Debug, Default, PartialEq)]
/// 用于表示一个设备的配置空间信息，通过这个结构体可以获取设备所占的内存页数和实际使用的内存页数
pub(crate) struct ConfigSpace {
    /// pub(crate) 表示这个结构体只能在当前 crate 中被公开访问，对于外部 crate 不可见
    pub num_pages: u32,
    pub actual_pages: u32,
}

// SAFETY: Safe because ConfigSpace only contains plain data.
/// ByteValued，用于指示一个类型在内存中表示为连续的字节序列，并且可以使用字节级别的操作来修改和访问这个类型的值。
/// 因为 ConfigSpace 只包含普通数据，即不包含 Rust 的不安全特性（如指针、裸指针、裸指针的解引用等等），因此可以保证在内存中表示为连续的字节序列
/// 需要注意的是，虽然这段代码本身被标记为“安全”（unsafe impl），但是仍然存在一定的潜在风险。因为在 Rust 中，
/// unsafe 代码允许对内存进行更底层、更直接的操作，而这些操作可能会违反 Rust 的所有权和借用系统，导致内存安全方面的问题。
/// 因此，一般来说，开发者在编写 unsafe 代码时必须保证其代码正确性和安全性，否则可能会导致不可预期的后果。
unsafe impl ByteValued for ConfigSpace {}

// This structure needs the `packed` attribute, otherwise Rust will assume
// the size to be 16 bytes.
#[derive(Copy, Clone, Debug, Default)]
/// #[repr(C, packed)]：这个元属性告诉编译器按照 C 语言的结构体定义方式来布局这个结构体的字段。
/// 其中 packed 表示告诉编译器不要为了对齐而填充字节。如果不加 packed，编译器会默认按照 8 字节的倍数来对齐字段，导致这个结构体的大小是 16 字节，而不是实际需要的 10 字节。
/// 这个结构体通常是用作气球（Balloon）的统计信息，也可能在其他场景下使用，比如和硬件打交道时需要处理原始二进制数据。
/// 为了确保不会因为编译器的结构体布局差异而导致代码行为异常，使用 repr(C) 和 packed 是一个很好的做法。
#[repr(C, packed)]
/// The statistics tags. 对应了下面的struct BalloonStats
/// const VIRTIO_BALLOON_S_SWAP_IN: u16 = 0;
/// const VIRTIO_BALLOON_S_SWAP_OUT: u16 = 1;
/// const VIRTIO_BALLOON_S_MAJFLT: u16 = 2;
/// const VIRTIO_BALLOON_S_MINFLT: u16 = 3;
/// const VIRTIO_BALLOON_S_MEMFREE: u16 = 4;
/// const VIRTIO_BALLOON_S_MEMTOT: u16 = 5;
/// const VIRTIO_BALLOON_S_AVAIL: u16 = 6;
/// const VIRTIO_BALLOON_S_CACHES: u16 = 7;
/// const VIRTIO_BALLOON_S_HTLB_PGALLOC: u16 = 8;
/// const VIRTIO_BALLOON_S_HTLB_PGFAIL: u16 = 9;
struct BalloonStat {
    pub tag: u16,
    pub val: u64,
}

// SAFETY: Safe because BalloonStat only contains plain data.
unsafe impl ByteValued for BalloonStat {}

// BalloonStats holds statistics returned from the stats_queue.
/// Serialize trait 则用于将一个结构体序列化成字节序列，方便存储或传输数据
/// PartialEq 和 Eq 都是 Rust 中的 trait，都用于比较两个值是否相等。它们的区别在于 Eq 是 PartialEq 的子集，
/// 即 Eq trait 要求实现的 PartialEq 方法还需要满足传递性（transitivity）：如果 A == B 且 B == C，则 A == C。
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize)]
pub struct BalloonConfig {
    pub amount_mib: u32,
    // 表示气球内存大小（以 MiB 为单位）
    pub deflate_on_oom: bool,
    // 在 Out Of Memory（OOM，内存不足）时是否启用"收紧气球"
    pub stats_polling_interval_s: u16, // 轮询统计信息的时间间隔（以秒为单位）
}

// BalloonStats holds statistics returned from the stats_queue.
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize)]
/// 这个属性是用在 Rust 的序列化/反序列化库 serde 上的，它的作用是告诉 serde 在反序列化时不要忽略掉任何未知的字段。
/// 如果数据格式中包含了未知的字段，而没有使用 #[serde(deny_unknown_fields)] 属性的话，在反序列化时 serde 会默默地忽略掉这些字段，
/// 但如果使用了这个属性，serde 就会抛出错误，通知我们输入的数据格式中包含了未知字段。
/// 这个属性一般用在反序列化时，特别是在处理外部输入的数据时会非常有用。它可以让我们更加严格地验证输入的数据，避免一些意外情况的发生。
/// 同时，对于一些已经规定好数据格式的应用中，使用这个属性也可以帮助我们快速发现问题，比如数据格式的变化带来的兼容性问题。
#[serde(deny_unknown_fields)]
pub struct BalloonStats {
    /// 注意，前四项是通过ConfigSpace进行更新和转化的，后面的内容，是由Guest提供的
    /// 目标页数和实际页数
    pub target_pages: u32,
    pub actual_pages: u32,
    /// 目标内存（以 MiB 为单位）和实际内存
    pub target_mib: u32,
    pub actual_mib: u32,
    /// 用于记录交换（swap in/out）的数据
    #[serde(skip_serializing_if = "Option::is_none")] /// 通过 Serialize 特性序列化成 JSON 格式时，若字段的值为 None，则会跳过序列化
    pub swap_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_out: Option<u64>,
    /// 用于记录主页面故障（major fault）和次页面故障（minor fault）的次数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub major_faults: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minor_faults: Option<u64>,
    /// 记录空闲、总共和可用内存的大小
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_memory: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_memory: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_memory: Option<u64>,
    /// 用于缓存磁盘数据的内存大小
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_caches: Option<u64>,
    /// 用于记录大页（huge pages）的分配和失败次数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hugetlb_allocations: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hugetlb_failures: Option<u64>,
}

impl BalloonStats {
    /// 用来更新结构体中的字段值。将输入的BalloonStat，更新到BalloonStats结构体中
    /// 该方法的输入参数是一个 &BalloonStat 类型的引用，输出结果是一个 Result 类型，如果更新操作成功，返回 Ok(())，否则返回 Err(BalloonError::MalformedPayload)。
    fn update_with_stat(&mut self, stat: &BalloonStat) -> Result<(), BalloonError> {
        let val = Some(stat.val);
        match stat.tag {
            VIRTIO_BALLOON_S_SWAP_IN => self.swap_in = val,
            VIRTIO_BALLOON_S_SWAP_OUT => self.swap_out = val,
            VIRTIO_BALLOON_S_MAJFLT => self.major_faults = val,
            VIRTIO_BALLOON_S_MINFLT => self.minor_faults = val,
            VIRTIO_BALLOON_S_MEMFREE => self.free_memory = val,
            VIRTIO_BALLOON_S_MEMTOT => self.total_memory = val,
            VIRTIO_BALLOON_S_AVAIL => self.available_memory = val,
            VIRTIO_BALLOON_S_CACHES => self.disk_caches = val,
            VIRTIO_BALLOON_S_HTLB_PGALLOC => self.hugetlb_allocations = val,
            VIRTIO_BALLOON_S_HTLB_PGFAIL => self.hugetlb_failures = val,
            _ => {
                return Err(BalloonError::MalformedPayload);
            }
        }

        Ok(())
    }
}

// Virtio balloon device.
pub struct Balloon {
    // Virtio fields.
    pub(crate) avail_features: u64,
    /// 表示设备支持的功能，其中 avail_features 是未被确认的功能，acked_features 是已经确认支持的功能。
    pub(crate) acked_features: u64,
    pub(crate) config_space: ConfigSpace,
    /// 表示设备的配置空间，包含一些指定设备参数的字段: 设备所占的内存页数和实际使用的内存页数
    pub(crate) activate_evt: EventFd,
    /// 激活设备的事件, 可以使用 EventFd 监视文件描述符，当它们发生变化时，就会触发事件。这个功能在 Unix 和 Linux 操作系统中被广泛使用，比如在网络编程中，监听文件描述符上的数据是否可读/可写。

    // Transport related fields.
    pub(crate) queues: Vec<Queue>,
    // 表示设备的消息队列，其中 queues 是消息队列的描述符，queue_evts 是消息队列的事件描述符。
    pub(crate) queue_evts: [EventFd; NUM_QUEUES],
    // [EventFd; NUM_QUEUES] 是一个 Rust 数组类型，它包含了 NUM_QUEUES 个 EventFd 对象。
    pub(crate) device_state: DeviceState,
    // 表示设备的状态，比如设备是否激活、是否可接受消息等。
    pub(crate) irq_trigger: IrqTrigger, // 表示设备的中断触发器

    // Implementation specific fields.
    pub(crate) restored: bool,
    // 表示设备是否已经恢复过。
    pub(crate) stats_polling_interval_s: u16,
    // 表示统计信息轮询的时间间隔，单位为秒。
    pub(crate) stats_timer: TimerFd,
    // 表示统计信息轮询定时器。
    // The index of the previous stats descriptor is saved because
    // it is acknowledged after the stats queue is processed.
    pub(crate) stats_desc_index: Option<u16>,
    // 表示上一次处理的统计信息描述符的索引，这个索引在统计信息队列被处理后会被确认。
    pub(crate) latest_stats: BalloonStats,
    // 表示最新的设备统计信息。
    // A buffer used as pfn accumulator during descriptor processing.
    pub(crate) pfn_buffer: [u32; MAX_PAGE_COMPACT_BUFFER], // 表示在描述符处理过程中用作页面帧号累加器的缓冲区。
}

impl Balloon {
    /// 这段 Rust 代码定义了一个名为 `new` 的 pub 方法，它接受四个参数 `amount_mib`, `deflate_on_oom`, `stats_polling_interval_s`, `restored`，并返回一个 `Result<Balloon, BalloonError>` 类型的结果。
    //
    /// 代码中首先创建了一个 `queue_evts` 数组，它包含三个 `EventFd` 对象，并配置为非阻塞模式。
    /// `EventFd` 对象用于通知 Balloon 设备块哪些事件已经发生，并允许 Balloon 设备块阻塞等待这些事件。
    /// 然后，使用 `QUEUE_SIZES` 数组创建了一个 `queues` 向量，它包含三个 `Queue` 对象，分别对应 Balloon 设备块的三个队列。
    /// 在这个向量创建之后，如果统计信息轮询间隔 `stats_polling_interval_s` 等于零，就从队列中删除处理器使用的每个统计信息队列。
    /// 否则，将其标记为启用，这将设置另一个标志位来表示是否启用了统计信息队列。
    //
    /// 接下来，定义了一个名为 `stats_timer` 的 `TimerFd` 对象，它用于定期轮询 Balloon 设备块以获取它当前的统计信息。
    /// 如果发生错误，则方法会返回 `BalloonError::Timer` 作为错误结果。
    //
    /// 接下来定义了一个名为 `Balloon` 的实例对象并返回它。在 Balloon 实例对象的构造过程中，将上文的 `queue_evts` 数组和 `queues` 向量初始化到实例对象中。
    /// 除此之外，还初始化了一些其他的字段，包括 `avail_features`、`acked_features`、`config_space` 等等，这些字段闯入在 `Balloon` 结构体中。
    /// 如果构造函数执行过程中发生错误，则将错误封装为 `BalloonError`，并通过 `Result<Balloon, BalloonError>` 返回错误结果。
    pub fn new(
        amount_mib: u32,
        deflate_on_oom: bool,
        stats_polling_interval_s: u16,
        restored: bool,
    ) -> Result<Balloon, BalloonError> {
        let mut avail_features = 1u64 << VIRTIO_F_VERSION_1;

        if deflate_on_oom {
            avail_features |= 1u64 << VIRTIO_BALLOON_F_DEFLATE_ON_OOM;
        };

        if stats_polling_interval_s > 0 {
            avail_features |= 1u64 << VIRTIO_BALLOON_F_STATS_VQ;
        }

        // 给每个队列挂上一个eventFD，和pistache中的队列设计完全一样
        let queue_evts = [
            EventFd::new(libc::EFD_NONBLOCK).map_err(BalloonError::EventFd)?,
            EventFd::new(libc::EFD_NONBLOCK).map_err(BalloonError::EventFd)?,
            EventFd::new(libc::EFD_NONBLOCK).map_err(BalloonError::EventFd)?,
        ];

        // QUEUE_SIZES中记录了每个队列的大小
        // 其中 QUEUE_SIZES 是一个包含多个 u16 类型数据的数组，表示每个队列的大小。iter() 方法用于返回一个表示数组元素序列的迭代器，
        // map() 方法对迭代器的每个元素应用给定的闭包函数进行转换，而在这里闭包函数的作用是将每个队列的大小作为参数创建一个新的 Queue 类型的实例，
        // 最后通过 collect() 方法将转换后的所有实例收集到一个 Vec 容器中。
        let mut queues: Vec<Queue> = QUEUE_SIZES.iter().map(|&s| Queue::new(s)).collect();

        // The VirtIO specification states that the statistics queue should
        // not be present at all if the statistics are not enabled.
        if stats_polling_interval_s == 0 {
            let _ = queues.remove(STATS_INDEX);
        }

        // TimerFD 时间轮询器
        let stats_timer =
            TimerFd::new_custom(ClockId::Monotonic, true, true).map_err(BalloonError::Timer)?;

        Ok(Balloon {
            avail_features,
            acked_features: 0u64,
            config_space: ConfigSpace {
                num_pages: mib_to_pages(amount_mib)?, // 气球设备的页面数
                actual_pages: 0, // 气球设备的实际页面数
            },
            queue_evts,
            queues,
            irq_trigger: IrqTrigger::new().map_err(BalloonError::EventFd)?,
            device_state: DeviceState::Inactive,
            /// 初始设备的状态为未激活
            activate_evt: EventFd::new(libc::EFD_NONBLOCK).map_err(BalloonError::EventFd)?,
            /// 用于激活的event
            restored,
            stats_polling_interval_s,
            stats_timer,
            stats_desc_index: None,
            latest_stats: BalloonStats::default(),
            pfn_buffer: [0u32; MAX_PAGE_COMPACT_BUFFER],
        })
    }

    /// 以下是 Balloon 设备块的 Rust 实现中，四个处理队列事件的方法。
    /// 这四个方法分别是 process_inflate_queue_event()、process_deflate_queue_event()、process_stats_queue_event() 和 process_stats_timer_event()。
    /// 它们的共同作用是监控对应队列的事件（EventFd），并在其中发现新事件时执行相应的操作。
    ///
    /// 每个方法都返回一个 Result 类型的值，其中包含了可能发生的 BalloonError（一个自定义的错误类型）以及方法执行后得到的结果。每个方法的实现非常相似，都包含以下三个步骤：
    ///
    /// 1. 通过 queue_evts 数组获取对应事件的 EventFd（分别为 INFLATE_INDEX、DEFLATE_INDEX 和 STATS_INDEX）。这个数组记录了 Balloon 设备块中所有 EventFd 句柄；
    /// 2. 使用 read() 函数等待一个事件的发生。如果在等待时出现错误，则直接将错误通过 map_err() 函数转换成 BalloonError 类型的错误并返回；
    /// 3. 根据方法名，分别调用 process_inflate_queue()、process_deflate_queue()、process_stats_queue() 或 trigger_stats_update() 函数进行队列处理。
    pub(crate) fn process_inflate_queue_event(&mut self) -> Result<(), BalloonError> {
        // BalloonError::EventFd 是一个自定义的错误类型，表示 EventFd 的创建和操作失败。
        // map_err(BalloonError::EventFd) 的作用是将可能在 EventFd 创建和操作过程中出现的错误转换为 BalloonError::EventFd 类型的错误。
        // ? 运算符用于在错误出现时快速返回并传播错误，它的作用类似于 try catch 语句。如果结果是 Ok，则该运算符将返回 Ok 中的值，否则将立即返回错误。
        // 因此，这行代码表示，如果 map_err 返回错误，将立即返回错误，否则继续执行下面的代码。
        self.queue_evts[INFLATE_INDEX]
            .read()
            .map_err(BalloonError::EventFd)?;
        self.process_inflate_queue()
    }

    pub(crate) fn process_deflate_queue_event(&mut self) -> Result<(), BalloonError> {
        self.queue_evts[DEFLATE_INDEX]
            .read()
            .map_err(BalloonError::EventFd)?;
        self.process_deflate_queue()
    }

    pub(crate) fn process_stats_queue_event(&mut self) -> Result<(), BalloonError> {
        self.queue_evts[STATS_INDEX]
            .read()
            .map_err(BalloonError::EventFd)?;
        self.process_stats_queue()
    }

    pub(crate) fn process_stats_timer_event(&mut self) -> Result<(), BalloonError> {
        self.stats_timer.read();
        self.trigger_stats_update()
    }


    /// 这段代码实现了 BalloonDevice 中的 process_inflate_queue 函数。当 BalloonDevice 接收到来自 VM 的膨胀请求时，process_inflate_queue 函数会被调用来处理这个请求。
    ///
    /// 函数的实现主要分为以下几个步骤：
    ///
    /// 1. 通过 self.device_state.mem() 获取到 VM 的内存空间。
    /// 2. 更新 METRICS 相关的数据。
    /// 3. 获取到inflate queue。
    /// 4. 通过 while 循环，逐一处理队列中的 Descriptor Chain。
    /// 5. 对每一个 Descriptor Chain 进行处理，将 PFN（Page Frame Number）加入到 pfn_buffer 中，并对其进行合法性检查。
    /// 6. 在每个 Descriptor Chain 处理完成后，对 pfn_buffer 中的所有 PFN 进行压缩处理，将相同连续的 PFN 进行合并。
    /// 7. 依次移除队列中的每个连续的 PFN，直到队列中所有连续的 PFN 均移除。
    /// 8. 如果标志位 needs_interrupt 被标记为 true，需要向 VM 发送中断信号。
    /// 在这个过程中，整个函数体贯穿着对错误类型 BalloonError 的处理，具体包括解包、封装、抛出等操作，以保证程序在出现任何错误时能够正常退出并返回相应的错误信息。
    ///
    pub(crate) fn process_inflate_queue(&mut self) -> Result<(), BalloonError> {
        // This is safe since we checked in the event handler that the device is activated.
        // device_state，指示Balloon 设备是否被激活，激活时需要提供用于表示设备所附加的内存区域的GuestMemoryMmap 的参数，这里的.mem()就是返回这个
        // self.device_state.mem() 返回了一个 Option 类型的值，表示可能存在一个内存区域。但在这里，我们通过 unwrap() 方法解包了这个值，也就是说，
        // 如果 self.device_state.mem() 返回了 None，那么程序会崩溃并抛出一个 panic。但是，由于前面的事件处理程序已经检查了该设备是否已经激活，所以这里使用 unwrap() 方法是安全的。
        let mem = self.device_state.mem().unwrap();
        METRICS.balloon.inflate_count.inc();

        let queue = &mut self.queues[INFLATE_INDEX];
        // The pfn buffer index used during descriptor processing.
        let mut pfn_buffer_idx = 0;
        let mut needs_interrupt = false;
        let mut valid_descs_found = true;

        // Loop until there are no more valid DescriptorChains.
        while valid_descs_found {
            valid_descs_found = false;
            // Internal loop processes descriptors and acummulates the pfns in `pfn_buffer`.
            // Breaks out when there is not enough space in `pfn_buffer` to completely process
            // the next descriptor.
            // 循环地从队列中取走IO请求，即一个Descriptor的链表,返回值为链表的头，数据类型为即一个DescriptorChain
            // 需要注意的是，这段循环的代码是存在Bug的，即每个 queue.pop(mem) ， 得到的都是一个链表，而非一个Descriptor(对应于结构体DescriptorChain)
            // 而head正是链表的头部，因此按道理应该是，从head遍历整个链表来获取完整的IO请求，但是在下面的代码实现中，并没有对链表进行遍历，而仅仅是读取了
            // head的内容。尽管如此，这段代码并不会出问题，因为Linux内核，会将每1MB的page，即256个PFN作为一次IO请求，写入到Queue中。因此每个IO请求
            // 的Descriptor链表，确实只有一个Descriptor，因此不需要对其进行遍历
            // （一个IO请求，对应了Linux内核中的一个散列表，Linux balloon使用了sg_init_one来初始化，所以其散列表中只有一个Descriptor）
            while let Some(head) = queue.pop(mem) {
                println!("{:?}", head);
                let len = head.len as usize; // 获取该Descriptor的数据区的大小，数据区存放的是guest返回的PFN
                /*
                 * 需要知道，在Linux内核中，用于传输的PFN的数据结构为：
                 * __virtio32 pfns[VIRTIO_BALLOON_ARRAY_PFNS_MAX];
                 * 因此，这个数据的类型就是U32，而这个数组的大小是：
                 * #define VIRTIO_BALLOON_ARRAY_PFNS_MAX 256
                 */
                let max_len = MAX_PAGES_IN_DESC * SIZE_OF_U32; // 每个Descriptor最多存放256个PFN，也即1MB
                valid_descs_found = true;

                // head的数据区就是内核传输过来的pfns数组，因此其数据区的长度一定是整除SIZE_OF_U32的
                // is_write_only 为真表明，这个descriptors对于Device是write_only,而对于driver是read_only，显然在这里，应该对于firecracker应该是只读的
                if !head.is_write_only() && len % SIZE_OF_U32 == 0 { //
                    // Check descriptor pfn count.
                    // head的长度肯定不能超过最大的长度限制，即其最多存放256个pfn
                    if len > max_len {
                        error!(
                            "Inflate descriptor has bogus page count {} > {}, skipping.",
                            len / SIZE_OF_U32,
                            MAX_PAGES_IN_DESC
                        );

                        // Skip descriptor.
                        continue;
                    }
                    // Break loop if `pfn_buffer` will be overrun by adding all pfns from current
                    // desc.
                    // firecracker会将所有要释放的pfn统一到一个pfn_buffer中，然后进行收缩处理，即尝试识别连续的pfn
                    // pfn_buffer的大小是MAX_PAGE_COMPACT_BUFFER=2048，当个pfn_buffer的大小不足以装下本次循环的
                    // Descriptor中的fpn时，将会退出循环，然后处理这一批的pfn，注意我们前面设置了valid_descs_found = true;
                    // 因此当上一批的fpn处理完成后，循环将会继续
                    if MAX_PAGE_COMPACT_BUFFER - pfn_buffer_idx < len / SIZE_OF_U32 {
                        queue.undo_pop();
                        break;
                    }

                    // This is safe, `len` was validated above.
                    // 循环的遍历出Descriptor的数据区中所有的pfn
                    for index in (0..len).step_by(SIZE_OF_U32) {
                        // head.addr 是数据区的首地址，加上index后，就是每个fpn的地址，整个地址是虚拟机的物理地址
                        let addr = head
                            .addr
                            .checked_add(index as u64)
                            .ok_or(BalloonError::MalformedDescriptor)?;

                        // 通过mem.read_obj，将pfn读出来
                        let page_frame_number = mem
                            .read_obj::<u32>(addr)
                            .map_err(|_| BalloonError::MalformedDescriptor)?;

                        // 将每个pfn加入到pfn_buffer中
                        self.pfn_buffer[pfn_buffer_idx] = page_frame_number;
                        pfn_buffer_idx += 1;
                    }
                }

                // Acknowledge the receipt of the descriptor.
                // 0 is number of bytes the device has written to memory.
                // 告诉guest，我们已经读取完成了一个IO请求，其可以将指定的descriptor给释放掉。
                queue
                    .add_used(mem, head.index, 0)
                    .map_err(BalloonError::Queue)?;
                needs_interrupt = true;
            }

            // Compact pages into ranges.
            // 将连续的pfn给合并，放入到page_ranges中，同时pfn_buffer被清空
            let page_ranges = compact_page_frame_numbers(&mut self.pfn_buffer[..pfn_buffer_idx]);
            pfn_buffer_idx = 0;

            // Remove the page ranges.
            // 通过pfn，获取其对应的虚拟机的物理内存地址，以及待释放范围的长度
            // 首先根据该地址找到物理机虚拟内存的地址，然后使用madvise的MADV_DONTNEED操作将指定的内存映射给取消
            for (page_frame_number, range_len) in page_ranges {
                let guest_addr =
                    GuestAddress(u64::from(page_frame_number) << VIRTIO_BALLOON_PFN_SHIFT);

                if let Err(err) = remove_range(
                    mem,
                    (guest_addr, u64::from(range_len) << VIRTIO_BALLOON_PFN_SHIFT),
                    self.restored,
                ) {
                    error!("Error removing memory range: {:?}", err);
                }
            }
        }
        // 告诉虚拟机，我们已经完成了对一组pfn的释放，通常就是释放了1MB，因为Linux内核只有在收到VMM的回信之后才会发下一组pfn
        if needs_interrupt {
            self.signal_used_queue()?;
        }

        Ok(())
    }

    // 对于收缩气球，也就是扩展VM的内存，firecracker是没有进行任何操作的，也就是，完全靠pagefault来填充物理内存
    // 因为对于使用MADV_DONTNEED的私有匿名页而言，下一次读会重新的分配物理内存，并按零填充
    pub(crate) fn process_deflate_queue(&mut self) -> Result<(), BalloonError> {
        // This is safe since we checked in the event handler that the device is activated.
        let mem = self.device_state.mem().unwrap();
        METRICS.balloon.deflate_count.inc();

        let queue = &mut self.queues[DEFLATE_INDEX];
        let mut needs_interrupt = false;

        while let Some(head) = queue.pop(mem) {
            queue
                .add_used(mem, head.index, 0)
                .map_err(BalloonError::Queue)?;
            needs_interrupt = true;
        }

        if needs_interrupt {
            self.signal_used_queue()
        } else {
            Ok(())
        }
    }

    pub(crate) fn process_stats_queue(&mut self) -> Result<(), BalloonError> {
        // This is safe since we checked in the event handler that the device is activated.
        let mem = self.device_state.mem().unwrap();
        METRICS.balloon.stats_updates_count.inc();

        while let Some(head) = self.queues[STATS_INDEX].pop(mem) {
            if let Some(prev_stats_desc) = self.stats_desc_index {
                // We shouldn't ever have an extra buffer if the driver follows
                // the protocol, but return it if we find one.
                error!("balloon: driver is not compliant, more than one stats buffer received");
                self.queues[STATS_INDEX]
                    .add_used(mem, prev_stats_desc, 0)
                    .map_err(BalloonError::Queue)?;
            }
            for index in (0..head.len).step_by(SIZE_OF_STAT) {
                // Read the address at position `index`. The only case
                // in which this fails is if there is overflow,
                // in which case this descriptor is malformed,
                // so we ignore the rest of it.
                let addr = head
                    .addr
                    .checked_add(u64::from(index))
                    .ok_or(BalloonError::MalformedDescriptor)?;
                let stat = mem
                    .read_obj::<BalloonStat>(addr)
                    .map_err(|_| BalloonError::MalformedDescriptor)?;
                self.latest_stats.update_with_stat(&stat).map_err(|_| {
                    METRICS.balloon.stats_update_fails.inc();
                    BalloonError::MalformedPayload
                })?;
            }

            self.stats_desc_index = Some(head.index);
        }

        Ok(())
    }

    pub(crate) fn signal_used_queue(&self) -> Result<(), BalloonError> {
        self.irq_trigger.trigger_irq(IrqType::Vring).map_err(|err| {
            METRICS.balloon.event_fails.inc();
            BalloonError::InterruptError(err)
        })
    }

    /// Process device virtio queue(s).
    pub fn process_virtio_queues(&mut self) {
        let _ = self.process_inflate_queue();
        let _ = self.process_deflate_queue();
    }

    pub fn id(&self) -> &str {
        BALLOON_DEV_ID
    }

    // 周期性的告诉guest，获取的states信息
    fn trigger_stats_update(&mut self) -> Result<(), BalloonError> {
        // This is safe since we checked in the event handler that the device is activated.
        let mem = self.device_state.mem().unwrap();

        // The communication is driven by the device by using the buffer
        // and sending a used buffer notification
        if let Some(index) = self.stats_desc_index.take() {
            self.queues[STATS_INDEX]
                .add_used(mem, index, 0)
                .map_err(BalloonError::Queue)?;
            self.signal_used_queue()
        } else {
            error!("Failed to update balloon stats, missing descriptor.");
            Ok(())
        }
    }

    pub fn update_size(&mut self, amount_mib: u32) -> Result<(), BalloonError> {
        if self.is_activated() {
            // 这个指令非常的关键，vmm通过配置空间，向guest传达，我希望将气球调节至多大，因此需要写入config_space.num_pages
            // guest会读取此数值，然后根据当前气球的大小进行调整，并将最终的实际调节结果写入到config_space.actul_pages
            self.config_space.num_pages = mib_to_pages(amount_mib)?;
            self.irq_trigger
                .trigger_irq(IrqType::Config)
                .map_err(BalloonError::InterruptError)
        } else {
            Err(BalloonError::DeviceNotActive)
        }
    }

    // 当用户改变stats_polling_interval的配置时，会由src/vmm/src/lib.rs中的update_balloon_stats_config函数调用该函数
    pub fn update_stats_polling_interval(&mut self, interval_s: u16) -> Result<(), BalloonError> {
        if self.stats_polling_interval_s == interval_s {
            return Ok(());
        }

        if self.stats_polling_interval_s == 0 || interval_s == 0 {
            return Err(BalloonError::StatisticsStateChange);
        }

        self.trigger_stats_update()?;

        self.stats_polling_interval_s = interval_s;
        self.update_timer_state();
        Ok(())
    }

    pub fn update_timer_state(&mut self) {
        let timer_state = TimerState::Periodic {
            current: Duration::from_secs(u64::from(self.stats_polling_interval_s)),
            interval: Duration::from_secs(u64::from(self.stats_polling_interval_s)),
        };
        self.stats_timer
            .set_state(timer_state, SetTimeFlags::Default);
    }

    pub fn num_pages(&self) -> u32 {
        self.config_space.num_pages
    }

    pub fn size_mb(&self) -> u32 {
        pages_to_mib(self.config_space.num_pages)
    }

    pub fn deflate_on_oom(&self) -> bool {
        self.avail_features & (1u64 << VIRTIO_BALLOON_F_DEFLATE_ON_OOM) != 0
    }

    pub fn stats_polling_interval_s(&self) -> u16 {
        self.stats_polling_interval_s
    }

    pub fn latest_stats(&mut self) -> Option<&BalloonStats> {
        if self.stats_enabled() {
            self.latest_stats.target_pages = self.config_space.num_pages;
            self.latest_stats.actual_pages = self.config_space.actual_pages;
            self.latest_stats.target_mib = pages_to_mib(self.latest_stats.target_pages);
            self.latest_stats.actual_mib = pages_to_mib(self.latest_stats.actual_pages);
            Some(&self.latest_stats)
        } else {
            None
        }
    }

    pub fn config(&self) -> BalloonConfig {
        BalloonConfig {
            amount_mib: self.size_mb(),
            deflate_on_oom: self.deflate_on_oom(),
            stats_polling_interval_s: self.stats_polling_interval_s(),
        }
    }

    pub(crate) fn stats_enabled(&self) -> bool {
        self.stats_polling_interval_s > 0
    }

    pub(crate) fn set_stats_desc_index(&mut self, stats_desc_index: Option<u16>) {
        self.stats_desc_index = stats_desc_index;
    }
}

impl VirtioDevice for Balloon {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn device_type(&self) -> u32 {
        TYPE_BALLOON
    }

    fn queues(&self) -> &[Queue] {
        &self.queues
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.queues
    }

    fn queue_events(&self) -> &[EventFd] {
        &self.queue_evts
    }

    fn interrupt_evt(&self) -> &EventFd {
        &self.irq_trigger.irq_evt
    }

    fn interrupt_status(&self) -> Arc<AtomicUsize> {
        self.irq_trigger.irq_status.clone()
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_space_bytes = self.config_space.as_slice();
        let config_len = config_space_bytes.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }

        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(
                &config_space_bytes[offset as usize..cmp::min(end, config_len) as usize],
            )
            .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let data_len = data.len() as u64;
        let config_space_bytes = self.config_space.as_mut_slice();
        let config_len = config_space_bytes.len() as u64;
        if offset + data_len > config_len {
            error!("Failed to write config space");
            return;
        }
        config_space_bytes[offset as usize..(offset + data_len) as usize].copy_from_slice(data);
    }

    fn activate(&mut self, mem: GuestMemoryMmap) -> ActivateResult {
        self.device_state = DeviceState::Activated(mem);
        if self.activate_evt.write(1).is_err() {
            error!("Balloon: Cannot write to activate_evt");
            METRICS.balloon.activate_fails.inc();
            self.device_state = DeviceState::Inactive;
            return Err(super::super::ActivateError::BadActivate);
        }

        if self.stats_enabled() {
            self.update_timer_state();
        }

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::u32;

    use utils::vm_memory::GuestAddress;

    use super::super::CONFIG_SPACE_SIZE;
    use super::*;
    use crate::check_metric_after_block;
    use crate::devices::report_balloon_event_fail;
    use crate::devices::virtio::balloon::test_utils::{
        check_request_completion, invoke_handler_for_queue_event, set_request,
    };
    use crate::devices::virtio::test_utils::{default_mem, VirtQueue};
    use crate::devices::virtio::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};

    impl Balloon {
        pub(crate) fn set_queue(&mut self, idx: usize, q: Queue) {
            self.queues[idx] = q;
        }

        pub(crate) fn actual_pages(&self) -> u32 {
            self.config_space.actual_pages
        }

        pub fn update_num_pages(&mut self, num_pages: u32) {
            self.config_space.num_pages = num_pages;
        }

        pub fn update_actual_pages(&mut self, actual_pages: u32) {
            self.config_space.actual_pages = actual_pages;
        }
    }

    #[test]
    fn test_balloon_stat_size() {
        assert_eq!(SIZE_OF_STAT, 10);
    }

    #[test]
    fn test_update_balloon_stats() {
        // Test all feature combinations.
        let mut stats = BalloonStats {
            target_pages: 5120,
            actual_pages: 2560,
            target_mib: 20,
            actual_mib: 10,
            swap_in: Some(0),
            swap_out: Some(0),
            major_faults: Some(0),
            minor_faults: Some(0),
            free_memory: Some(0),
            total_memory: Some(0),
            available_memory: Some(0),
            disk_caches: Some(0),
            hugetlb_allocations: Some(0),
            hugetlb_failures: Some(0),
        };

        let mut stat = BalloonStat {
            tag: VIRTIO_BALLOON_S_SWAP_IN,
            val: 1,
        };

        stats.update_with_stat(&stat).unwrap();
        assert_eq!(stats.swap_in, Some(1));
        stat.tag = VIRTIO_BALLOON_S_SWAP_OUT;
        stats.update_with_stat(&stat).unwrap();
        assert_eq!(stats.swap_out, Some(1));
        stat.tag = VIRTIO_BALLOON_S_MAJFLT;
        stats.update_with_stat(&stat).unwrap();
        assert_eq!(stats.major_faults, Some(1));
        stat.tag = VIRTIO_BALLOON_S_MINFLT;
        stats.update_with_stat(&stat).unwrap();
        assert_eq!(stats.minor_faults, Some(1));
        stat.tag = VIRTIO_BALLOON_S_MEMFREE;
        stats.update_with_stat(&stat).unwrap();
        assert_eq!(stats.free_memory, Some(1));
        stat.tag = VIRTIO_BALLOON_S_MEMTOT;
        stats.update_with_stat(&stat).unwrap();
        assert_eq!(stats.total_memory, Some(1));
        stat.tag = VIRTIO_BALLOON_S_AVAIL;
        stats.update_with_stat(&stat).unwrap();
        assert_eq!(stats.available_memory, Some(1));
        stat.tag = VIRTIO_BALLOON_S_CACHES;
        stats.update_with_stat(&stat).unwrap();
        assert_eq!(stats.disk_caches, Some(1));
        stat.tag = VIRTIO_BALLOON_S_HTLB_PGALLOC;
        stats.update_with_stat(&stat).unwrap();
        assert_eq!(stats.hugetlb_allocations, Some(1));
        stat.tag = VIRTIO_BALLOON_S_HTLB_PGFAIL;
        stats.update_with_stat(&stat).unwrap();
        assert_eq!(stats.hugetlb_failures, Some(1));
    }

    #[test]
    fn test_virtio_features() {
        // Test all feature combinations.
        for deflate_on_oom in vec![true, false].iter() {
            for stats_interval in vec![0, 1].iter() {
                let mut balloon = Balloon::new(0, *deflate_on_oom, *stats_interval, false).unwrap();
                assert_eq!(balloon.device_type(), TYPE_BALLOON);

                let features: u64 = (1u64 << VIRTIO_F_VERSION_1)
                    | (u64::from(*deflate_on_oom) << VIRTIO_BALLOON_F_DEFLATE_ON_OOM)
                    | ((u64::from(*stats_interval)) << VIRTIO_BALLOON_F_STATS_VQ);

                assert_eq!(balloon.avail_features_by_page(0), features as u32);
                assert_eq!(balloon.avail_features_by_page(1), (features >> 32) as u32);
                for i in 2..10 {
                    assert_eq!(balloon.avail_features_by_page(i), 0u32);
                }

                for i in 0..10 {
                    balloon.ack_features_by_page(i, u32::MAX);
                }
                // Only present features should be acknowledged.
                assert_eq!(balloon.acked_features, features);
            }
        }
    }

    #[test]
    fn test_virtio_read_config() {
        let balloon = Balloon::new(0x10, true, 0, false).unwrap();

        let cfg = BalloonConfig {
            amount_mib: 16,
            deflate_on_oom: true,
            stats_polling_interval_s: 0,
        };
        assert_eq!(balloon.config(), cfg);

        let mut actual_config_space = [0u8; CONFIG_SPACE_SIZE];
        balloon.read_config(0, &mut actual_config_space);
        // The first 4 bytes are num_pages, the last 4 bytes are actual_pages.
        // The config space is little endian.
        // 0x10 MB in the constructor corresponds to 0x1000 pages in the
        // config space.
        let expected_config_space: [u8; CONFIG_SPACE_SIZE] =
            [0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(actual_config_space, expected_config_space);

        // Invalid read.
        let expected_config_space: [u8; CONFIG_SPACE_SIZE] =
            [0xd, 0xe, 0xa, 0xd, 0xb, 0xe, 0xe, 0xf];
        actual_config_space = expected_config_space;
        balloon.read_config(CONFIG_SPACE_SIZE as u64 + 1, &mut actual_config_space);

        // Validate read failed (the config space was not updated).
        assert_eq!(actual_config_space, expected_config_space);
    }

    #[test]
    fn test_virtio_write_config() {
        let mut balloon = Balloon::new(0, true, 0, false).unwrap();

        let expected_config_space: [u8; CONFIG_SPACE_SIZE] =
            [0x00, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        balloon.write_config(0, &expected_config_space);

        let mut actual_config_space = [0u8; CONFIG_SPACE_SIZE];
        balloon.read_config(0, &mut actual_config_space);
        assert_eq!(actual_config_space, expected_config_space);

        // Invalid write.
        let new_config_space = [0xd, 0xe, 0xa, 0xd, 0xb, 0xe, 0xe, 0xf];
        balloon.write_config(5, &new_config_space);
        // Make sure nothing got written.
        balloon.read_config(0, &mut actual_config_space);
        assert_eq!(actual_config_space, expected_config_space);
    }

    #[test]
    fn test_invalid_request() {
        let mut balloon = Balloon::new(0, true, 0, false).unwrap();
        let mem = default_mem();
        // Only initialize the inflate queue to demonstrate invalid request handling.
        let infq = VirtQueue::new(GuestAddress(0), &mem, 16);
        balloon.set_queue(INFLATE_INDEX, infq.create_queue());
        balloon.activate(mem.clone()).unwrap();

        // Fill the second page with non-zero bytes.
        for i in 0..0x1000 {
            assert!(mem.write_obj::<u8>(1, GuestAddress((1 << 12) + i)).is_ok());
        }

        // Will write the page frame number of the affected frame at this
        // arbitrary address in memory.
        let page_addr = 0x10;

        // Invalid case: the descriptor is write-only.
        {
            mem.write_obj::<u32>(0x1, GuestAddress(page_addr)).unwrap();
            set_request(
                &infq,
                0,
                page_addr,
                SIZE_OF_U32 as u32,
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            );

            invoke_handler_for_queue_event(&mut balloon, INFLATE_INDEX);
            check_request_completion(&infq, 0);

            // Check that the page was not zeroed.
            for i in 0..0x1000 {
                assert_eq!(mem.read_obj::<u8>(GuestAddress((1 << 12) + i)).unwrap(), 1);
            }
        }

        // Invalid case: descriptor len is not a multiple of 'SIZE_OF_U32'.
        {
            mem.write_obj::<u32>(0x1, GuestAddress(page_addr)).unwrap();
            set_request(
                &infq,
                1,
                page_addr,
                SIZE_OF_U32 as u32 + 1,
                VIRTQ_DESC_F_NEXT,
            );

            invoke_handler_for_queue_event(&mut balloon, INFLATE_INDEX);
            check_request_completion(&infq, 1);

            // Check that the page was not zeroed.
            for i in 0..0x1000 {
                assert_eq!(mem.read_obj::<u8>(GuestAddress((1 << 12) + i)).unwrap(), 1);
            }
        }
    }

    #[test]
    fn test_inflate() {
        let mut balloon = Balloon::new(0, true, 0, false).unwrap();
        let mem = default_mem();
        let infq = VirtQueue::new(GuestAddress(0), &mem, 16);
        balloon.set_queue(INFLATE_INDEX, infq.create_queue());
        balloon.activate(mem.clone()).unwrap();

        // Fill the third page with non-zero bytes.
        for i in 0..0x1000 {
            assert!(mem.write_obj::<u8>(1, GuestAddress((1 << 12) + i)).is_ok());
        }

        // Will write the page frame number of the affected frame at this
        // arbitrary address in memory.
        let page_addr = 0x10;

        // Error case: the request is well-formed, but we forgot
        // to trigger the inflate event queue.
        {
            mem.write_obj::<u32>(0x1, GuestAddress(page_addr)).unwrap();
            set_request(&infq, 0, page_addr, SIZE_OF_U32 as u32, VIRTQ_DESC_F_NEXT);

            check_metric_after_block!(
                METRICS.balloon.event_fails,
                1,
                balloon
                    .process_inflate_queue_event()
                    .unwrap_or_else(report_balloon_event_fail)
            );
            // Verify that nothing got processed.
            assert_eq!(infq.used.idx.get(), 0);

            // Check that the page was not zeroed.
            for i in 0..0x1000 {
                assert_eq!(mem.read_obj::<u8>(GuestAddress((1 << 12) + i)).unwrap(), 1);
            }
        }

        // Test the happy case.
        {
            mem.write_obj::<u32>(0x1, GuestAddress(page_addr)).unwrap();
            set_request(&infq, 0, page_addr, SIZE_OF_U32 as u32, VIRTQ_DESC_F_NEXT);

            check_metric_after_block!(
                METRICS.balloon.inflate_count,
                1,
                invoke_handler_for_queue_event(&mut balloon, INFLATE_INDEX)
            );
            check_request_completion(&infq, 0);

            // Check that the page was zeroed.
            for i in 0..0x1000 {
                assert_eq!(mem.read_obj::<u8>(GuestAddress((1 << 12) + i)).unwrap(), 0);
            }
        }
    }

    #[test]
    fn test_deflate() {
        let mut balloon = Balloon::new(0, true, 0, false).unwrap();
        let mem = default_mem();
        let defq = VirtQueue::new(GuestAddress(0), &mem, 16);
        balloon.set_queue(DEFLATE_INDEX, defq.create_queue());
        balloon.activate(mem.clone()).unwrap();

        let page_addr = 0x10;

        // Error case: forgot to trigger deflate event queue.
        {
            set_request(&defq, 0, page_addr, SIZE_OF_U32 as u32, VIRTQ_DESC_F_NEXT);
            check_metric_after_block!(
                METRICS.balloon.event_fails,
                1,
                balloon
                    .process_deflate_queue_event()
                    .unwrap_or_else(report_balloon_event_fail)
            );
            // Verify that nothing got processed.
            assert_eq!(defq.used.idx.get(), 0);
        }

        // Happy case.
        {
            set_request(&defq, 1, page_addr, SIZE_OF_U32 as u32, VIRTQ_DESC_F_NEXT);
            check_metric_after_block!(
                METRICS.balloon.deflate_count,
                1,
                invoke_handler_for_queue_event(&mut balloon, DEFLATE_INDEX)
            );
            check_request_completion(&defq, 1);
        }
    }

    #[test]
    fn test_stats() {
        let mut balloon = Balloon::new(0, true, 1, false).unwrap();
        let mem = default_mem();
        let statsq = VirtQueue::new(GuestAddress(0), &mem, 16);
        balloon.set_queue(STATS_INDEX, statsq.create_queue());
        balloon.activate(mem.clone()).unwrap();

        let page_addr = 0x100;

        // Error case: forgot to trigger stats event queue.
        {
            set_request(&statsq, 0, 0x1000, SIZE_OF_STAT as u32, VIRTQ_DESC_F_NEXT);
            check_metric_after_block!(
                METRICS.balloon.event_fails,
                1,
                balloon
                    .process_stats_queue_event()
                    .unwrap_or_else(report_balloon_event_fail)
            );
            // Verify that nothing got processed.
            assert_eq!(statsq.used.idx.get(), 0);
        }

        // Happy case.
        {
            let swap_out_stat = BalloonStat {
                tag: VIRTIO_BALLOON_S_SWAP_OUT,
                val: 0x1,
            };
            let mem_free_stat = BalloonStat {
                tag: VIRTIO_BALLOON_S_MEMFREE,
                val: 0x5678,
            };

            // Write the stats in memory.
            mem.write_obj::<BalloonStat>(swap_out_stat, GuestAddress(page_addr))
                .unwrap();
            mem.write_obj::<BalloonStat>(
                mem_free_stat,
                GuestAddress(page_addr + SIZE_OF_STAT as u64),
            )
            .unwrap();

            set_request(
                &statsq,
                0,
                page_addr,
                2 * SIZE_OF_STAT as u32,
                VIRTQ_DESC_F_NEXT,
            );
            check_metric_after_block!(METRICS.balloon.stats_updates_count, 1, {
                // Trigger the queue event.
                balloon.queue_events()[STATS_INDEX].write(1).unwrap();
                balloon.process_stats_queue_event().unwrap();
                // Don't check for completion yet.
            });

            let stats = balloon.latest_stats().unwrap();
            let expected_stats = BalloonStats {
                swap_out: Some(0x1),
                free_memory: Some(0x5678),
                ..BalloonStats::default()
            };
            assert_eq!(stats, &expected_stats);

            // Wait for the timer to expire, although as it is non-blocking
            // we could just process the timer event and it would not
            // return an error.
            std::thread::sleep(Duration::from_secs(1));
            check_metric_after_block!(METRICS.balloon.event_fails, 0, {
                // Trigger the timer event, which consumes the stats
                // descriptor index and signals the used queue.
                assert!(balloon.stats_desc_index.is_some());
                assert!(balloon.process_stats_timer_event().is_ok());
                assert!(balloon.stats_desc_index.is_none());
                assert!(balloon.irq_trigger.has_pending_irq(IrqType::Vring));
            });
        }
    }

    #[test]
    fn test_process_balloon_queues() {
        let mut balloon = Balloon::new(0x10, true, 0, false).unwrap();
        let mem = default_mem();
        balloon.activate(mem).unwrap();
        balloon.process_virtio_queues()
    }

    #[test]
    fn test_update_stats_interval() {
        let mut balloon = Balloon::new(0, true, 0, false).unwrap();
        let mem = default_mem();
        balloon.activate(mem).unwrap();
        assert_eq!(
            format!("{:?}", balloon.update_stats_polling_interval(1)),
            "Err(StatisticsStateChange)"
        );
        assert!(balloon.update_stats_polling_interval(0).is_ok());

        let mut balloon = Balloon::new(0, true, 1, false).unwrap();
        let mem = default_mem();
        balloon.activate(mem).unwrap();
        assert_eq!(
            format!("{:?}", balloon.update_stats_polling_interval(0)),
            "Err(StatisticsStateChange)"
        );
        assert!(balloon.update_stats_polling_interval(1).is_ok());
        assert!(balloon.update_stats_polling_interval(2).is_ok());
    }

    #[test]
    fn test_num_pages() {
        let mut balloon = Balloon::new(0, true, 0, false).unwrap();
        // Assert that we can't update an inactive device.
        assert!(balloon.update_size(1).is_err());
        // Switch the state to active.
        balloon.device_state = DeviceState::Activated(
            utils::vm_memory::test_utils::create_guest_memory_unguarded(
                &[(GuestAddress(0x0), 0x1)],
                false,
            )
            .unwrap(),
        );

        assert_eq!(balloon.num_pages(), 0);
        assert_eq!(balloon.actual_pages(), 0);

        // Update fields through the API.
        balloon.update_actual_pages(0x1234);
        balloon.update_num_pages(0x100);
        assert_eq!(balloon.num_pages(), 0x100);
        assert!(balloon.update_size(16).is_ok());

        let mut actual_config = vec![0; CONFIG_SPACE_SIZE];
        balloon.read_config(0, &mut actual_config);
        assert_eq!(actual_config, vec![0x0, 0x10, 0x0, 0x0, 0x34, 0x12, 0, 0]);
        assert_eq!(balloon.num_pages(), 0x1000);
        assert_eq!(balloon.actual_pages(), 0x1234);
        assert_eq!(balloon.size_mb(), 16);

        // Update fields through the config space.
        let expected_config = vec![0x44, 0x33, 0x22, 0x11, 0x78, 0x56, 0x34, 0x12];
        balloon.write_config(0, &expected_config);
        assert_eq!(balloon.num_pages(), 0x1122_3344);
        assert_eq!(balloon.actual_pages(), 0x1234_5678);
    }
}
