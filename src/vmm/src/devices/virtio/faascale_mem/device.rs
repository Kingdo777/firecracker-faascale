// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::cmp;
use std::io::Write;
use std::result::Result;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;
use log::debug;

use logger::{error, IncMetric, METRICS};
use serde::Serialize;
use timerfd::{ClockId, SetTimeFlags, TimerFd, TimerState};
use utils::eventfd::EventFd;
use utils::vm_memory::{Address, ByteValued, Bytes, GuestAddress, GuestMemoryMmap};
use virtio_gen::virtio_blk::VIRTIO_F_VERSION_1;

use super::super::{ActivateResult, DeviceState, Queue, VirtioDevice, TYPE_FAASCALE_MEM};
use super::util::{populate_range, remove_range};
use super::{
    FAASCALE_MEM_DEV_ID, POPULATE_INDEX, DEPOPULATE_INDEX,
    MIB_TO_4K_PAGES, NUM_QUEUES, QUEUE_SIZES, FAASCALE_STATS_INDEX,
    VIRTIO_FAASCALE_MEM_F_STATS_VQ, VIRTIO_FAASCALE_MEM_S_AVAIL, VIRTIO_FAASCALE_MEM_PFN_SHIFT,
    VIRTIO_FAASCALE_MEM_S_CACHES, VIRTIO_FAASCALE_MEM_S_HTLB_PGALLOC, VIRTIO_FAASCALE_MEM_S_HTLB_PGFAIL,
    VIRTIO_FAASCALE_MEM_S_MAJFLT, VIRTIO_FAASCALE_MEM_S_MEMFREE, VIRTIO_FAASCALE_MEM_S_MEMTOT,
    VIRTIO_FAASCALE_MEM_S_MINFLT, VIRTIO_FAASCALE_MEM_S_SWAP_IN, VIRTIO_FAASCALE_MEM_S_SWAP_OUT,
};
use crate::devices::virtio::faascale_mem::{Error as FaascaleMemError, MAX_BLOCKS_IN_DESC};
use crate::devices::virtio::{IrqTrigger, IrqType};

/// SIZE_OF_U32和SIZE_OF_STAT，分别表示u32和FaascaleMemStat类型的大小（以字节为单位）
const SIZE_OF_BLOCK_INFO: usize = std::mem::size_of::<(u32, u32)>();
/// std::mem::size_of函数来获取类型的大小
const SIZE_OF_STAT: usize = std::mem::size_of::<FaascaleMemStat>();

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
/// 这个结构体通常是用作统计信息，也可能在其他场景下使用，比如和硬件打交道时需要处理原始二进制数据。
/// 为了确保不会因为编译器的结构体布局差异而导致代码行为异常，使用 repr(C) 和 packed 是一个很好的做法。
#[repr(C, packed)]
/// The statistics tags. 对应了下面的struct FaascaleMemStat
/// const VIRTIO_FAASCALE_MEM_S_SWAP_IN: u16 = 0;
/// const VIRTIO_FAASCALE_MEM_S_SWAP_OUT: u16 = 1;
/// const VIRTIO_FAASCALE_MEM_S_MAJFLT: u16 = 2;
/// const VIRTIO_FAASCALE_MEM_S_MINFLT: u16 = 3;
/// const VIRTIO_FAASCALE_MEM_S_MEMFREE: u16 = 4;
/// const VIRTIO_FAASCALE_MEM_S_MEMTOT: u16 = 5;
/// const VIRTIO_FAASCALE_MEM_S_AVAIL: u16 = 6;
/// const VIRTIO_FAASCALE_MEM_S_CACHES: u16 = 7;
/// const VIRTIO_FAASCALE_MEM_S_HTLB_PGALLOC: u16 = 8;
/// const VIRTIO_FAASCALE_MEM_S_HTLB_PGFAIL: u16 = 9;
struct FaascaleMemStat {
    pub tag: u16,
    pub val: u64,
}

// SAFETY: Safe because FaascaleMemStat only contains plain data.
unsafe impl ByteValued for FaascaleMemStat {}

// FaascaleMemStats holds statistics returned from the stats_queue.
/// Serialize trait 则用于将一个结构体序列化成字节序列，方便存储或传输数据
/// PartialEq 和 Eq 都是 Rust 中的 trait，都用于比较两个值是否相等。它们的区别在于 Eq 是 PartialEq 的子集，
/// 即 Eq trait 要求实现的 PartialEq 方法还需要满足传递性（transitivity）：如果 A == B 且 B == C，则 A == C。
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize)]
pub struct FaascaleMemConfig {
    pub stats_polling_interval_s: u16, // 轮询统计信息的时间间隔（以秒为单位）
    pub pre_alloc_mem: bool,
    pub pre_tdp_fault: bool,
}

// FaascaleMemStats holds statistics returned from the stats_queue.
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize)]
/// 这个属性是用在 Rust 的序列化/反序列化库 serde 上的，它的作用是告诉 serde 在反序列化时不要忽略掉任何未知的字段。
/// 如果数据格式中包含了未知的字段，而没有使用 #[serde(deny_unknown_fields)] 属性的话，在反序列化时 serde 会默默地忽略掉这些字段，
/// 但如果使用了这个属性，serde 就会抛出错误，通知我们输入的数据格式中包含了未知字段。
/// 这个属性一般用在反序列化时，特别是在处理外部输入的数据时会非常有用。它可以让我们更加严格地验证输入的数据，避免一些意外情况的发生。
/// 同时，对于一些已经规定好数据格式的应用中，使用这个属性也可以帮助我们快速发现问题，比如数据格式的变化带来的兼容性问题。
#[serde(deny_unknown_fields)]
pub struct FaascaleMemStats {
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

impl FaascaleMemStats {
    /// 用来更新结构体中的字段值。将输入的FaascaleMemStat，更新到FaascaleMemStats结构体中
    /// 该方法的输入参数是一个 &FaascaleMemStat 类型的引用，输出结果是一个 Result 类型，如果更新操作成功，返回 Ok(())，否则返回 Err(FaascaleMemError::MalformedPayload)。
    fn update_with_stat(&mut self, stat: &FaascaleMemStat) -> Result<(), FaascaleMemError> {
        let val = Some(stat.val);
        match stat.tag {
            VIRTIO_FAASCALE_MEM_S_SWAP_IN => self.swap_in = val,
            VIRTIO_FAASCALE_MEM_S_SWAP_OUT => self.swap_out = val,
            VIRTIO_FAASCALE_MEM_S_MAJFLT => self.major_faults = val,
            VIRTIO_FAASCALE_MEM_S_MINFLT => self.minor_faults = val,
            VIRTIO_FAASCALE_MEM_S_MEMFREE => self.free_memory = val,
            VIRTIO_FAASCALE_MEM_S_MEMTOT => self.total_memory = val,
            VIRTIO_FAASCALE_MEM_S_AVAIL => self.available_memory = val,
            VIRTIO_FAASCALE_MEM_S_CACHES => self.disk_caches = val,
            VIRTIO_FAASCALE_MEM_S_HTLB_PGALLOC => self.hugetlb_allocations = val,
            VIRTIO_FAASCALE_MEM_S_HTLB_PGFAIL => self.hugetlb_failures = val,
            _ => {
                return Err(FaascaleMemError::MalformedPayload);
            }
        }

        Ok(())
    }
}

// Virtio FaascaleMem device.
pub struct FaascaleMem {
    // Virtio fields.
    pub(crate) avail_features: u64,
    /// 表示设备支持的功能，其中 avail_features 是未被确认的功能，acked_features 是已经确认支持的功能。
    pub(crate) acked_features: u64,
    pub(crate) config_space: ConfigSpace,
    /// 表示设备的配置空间，包含一些指定设备参数的字段: 设备所占的内存页数和实际使用的内存页数
    pub(crate) activate_evt: EventFd,
    /// 激活设备的事件, 可以使用 EventFd 监视文件描述符，当它们发生变化时，就会触发事件。这个功能在 Unix 和 Linux 操作系统中被广泛使用，比如在网络编程中，监听文件描述符上的数据是否可读/可写。

    // Transport related fields.
    // 表示设备的消息队列，其中 queues 是消息队列的描述符，queue_evts 是消息队列的事件描述符。
    pub(crate) queues: Vec<Queue>,
    // [EventFd; NUM_QUEUES] 是一个 Rust 数组类型，它包含了 NUM_QUEUES 个 EventFd 对象。
    pub(crate) queue_evts: [EventFd; NUM_QUEUES],
    // 表示设备的状态，比如设备是否激活、是否可接受消息等。
    pub(crate) device_state: DeviceState,
    // 表示设备的中断触发器
    pub(crate) irq_trigger: IrqTrigger,

    // Implementation specific fields.
    pub(crate) restored: bool,
    pub(crate) pre_alloc_mem: bool,
    pub(crate) pre_tdp_fault: bool,
    pub(crate) stats_polling_interval_s: u16,
    pub(crate) stats_timer: TimerFd,
    // The index of the previous stats descriptor is saved because
    // it is acknowledged after the stats queue is processed.
    pub(crate) stats_desc_index: Option<u16>,
    pub(crate) latest_stats: FaascaleMemStats,
}

impl FaascaleMem {
    pub fn new(
        stats_polling_interval_s: u16,
        restored: bool,
        pre_alloc_mem: bool,
        pre_tdp_fault: bool
    ) -> Result<FaascaleMem, FaascaleMemError> {
        let mut avail_features = 1u64 << VIRTIO_F_VERSION_1;

        if stats_polling_interval_s > 0 {
            avail_features |= 1u64 << VIRTIO_FAASCALE_MEM_F_STATS_VQ;
        }

        // 给每个队列挂上一个eventFD，和pistache中的队列设计完全一样
        let queue_evts = [
            EventFd::new(libc::EFD_NONBLOCK).map_err(FaascaleMemError::EventFd)?,
            EventFd::new(libc::EFD_NONBLOCK).map_err(FaascaleMemError::EventFd)?,
            EventFd::new(libc::EFD_NONBLOCK).map_err(FaascaleMemError::EventFd)?,
        ];

        // QUEUE_SIZES中记录了每个队列的大小
        // 其中 QUEUE_SIZES 是一个包含多个 u16 类型数据的数组，表示每个队列的大小。iter() 方法用于返回一个表示数组元素序列的迭代器，
        // map() 方法对迭代器的每个元素应用给定的闭包函数进行转换，而在这里闭包函数的作用是将每个队列的大小作为参数创建一个新的 Queue 类型的实例，
        // 最后通过 collect() 方法将转换后的所有实例收集到一个 Vec 容器中。
        let mut queues: Vec<Queue> = QUEUE_SIZES.iter().map(|&s| Queue::new(s)).collect();

        // The VirtIO specification states that the statistics queue should
        // not be present at all if the statistics are not enabled.
        if stats_polling_interval_s == 0 {
            let _ = queues.remove(FAASCALE_STATS_INDEX);
        }

        // TimerFD 时间轮询器
        let stats_timer =
            TimerFd::new_custom(ClockId::Monotonic, true, true).map_err(FaascaleMemError::Timer)?;

        Ok(FaascaleMem {
            avail_features,
            acked_features: 0u64,
            config_space: ConfigSpace {
                num_pages: 0, // 气球设备的页面数
                actual_pages: 0, // 气球设备的实际页面数
            },
            queue_evts,
            queues,
            irq_trigger: IrqTrigger::new().map_err(FaascaleMemError::EventFd)?,
            device_state: DeviceState::Inactive,
            /// 初始设备的状态为未激活
            activate_evt: EventFd::new(libc::EFD_NONBLOCK).map_err(FaascaleMemError::EventFd)?,
            /// 用于激活的event
            restored,
            pre_alloc_mem,
            pre_tdp_fault,
            stats_polling_interval_s,
            stats_timer,
            stats_desc_index: None,
            latest_stats: FaascaleMemStats::default(),
        })
    }


    pub(crate) fn process_populate_queue_event(&mut self) -> Result<(), FaascaleMemError> {
        // FaascaleMemError::EventFd 是一个自定义的错误类型，表示 EventFd 的创建和操作失败。
        // map_err(FaascaleMemError::EventFd) 的作用是将可能在 EventFd 创建和操作过程中出现的错误转换为 FaascaleMemError::EventFd 类型的错误。
        // ? 运算符用于在错误出现时快速返回并传播错误，它的作用类似于 try catch 语句。如果结果是 Ok，则该运算符将返回 Ok 中的值，否则将立即返回错误。
        // 因此，这行代码表示，如果 map_err 返回错误，将立即返回错误，否则继续执行下面的代码。
        self.queue_evts[POPULATE_INDEX]
            .read()
            .map_err(FaascaleMemError::EventFd)?;
        self.process_populate_queue(POPULATE_INDEX)
    }

    pub(crate) fn process_depopulate_queue_event(&mut self) -> Result<(), FaascaleMemError> {
        self.queue_evts[DEPOPULATE_INDEX]
            .read()
            .map_err(FaascaleMemError::EventFd)?;
        self.process_populate_queue(DEPOPULATE_INDEX)
    }

    pub(crate) fn process_stats_queue_event(&mut self) -> Result<(), FaascaleMemError> {
        self.queue_evts[FAASCALE_STATS_INDEX]
            .read()
            .map_err(FaascaleMemError::EventFd)?;
        self.process_stats_queue()
    }

    pub(crate) fn process_stats_timer_event(&mut self) -> Result<(), FaascaleMemError> {
        self.stats_timer.read();
        self.trigger_stats_update()
    }

    // 对于收缩气球，也就是扩展VM的内存，firecracker是没有进行任何操作的，也就是，完全靠pagefault来填充物理内存
    // 因为对于使用MADV_DONTNEED的私有匿名页而言，下一次读会重新的分配物理内存，并按零填充
    pub(crate) fn process_populate_queue(&mut self, queue_index: usize) -> Result<(), FaascaleMemError> {
        // This is safe since we checked in the event handler that the device is activated.
        // device_state，指示FaascaleMem 设备是否被激活，激活时需要提供用于表示设备所附加的内存区域的GuestMemoryMmap 的参数，这里的.mem()就是返回这个
        // self.device_state.mem() 返回了一个 Option 类型的值，表示可能存在一个内存区域。但在这里，我们通过 unwrap() 方法解包了这个值，也就是说，
        // 如果 self.device_state.mem() 返回了 None，那么程序会崩溃并抛出一个 panic。但是，由于前面的事件处理程序已经检查了该设备是否已经激活，所以这里使用 unwrap() 方法是安全的。
        let mem = self.device_state.mem().unwrap();
        METRICS.faascale_mem.depopulate_count.inc();

        let queue = &mut self.queues[queue_index];

        let mut needs_interrupt = false;

        // Internal loop processes descriptors and acummulates the pfns in `pfn_buffer`.
        // Breaks out when there is not enough space in `pfn_buffer` to completely process
        // the next descriptor.
        // 循环地从队列中取走IO请求，即一个Descriptor的链表,返回值为链表的头，数据类型为即一个DescriptorChain
        // 需要注意的是，这段循环的代码是存在Bug的，即每个 queue.pop(mem) ， 得到的都是一个链表，而非一个Descriptor(对应于结构体DescriptorChain)
        // 而head正是链表的头部，因此按道理应该是，从head遍历整个链表来获取完整的IO请求，但是在下面的代码实现中，并没有对链表进行遍历，而仅仅是读取了
        // head的内容。尽管如此，这段代码并不会出问题，因为Linux内核，会将每1MB的page，即256个PFN作为一次IO请求，写入到Queue中。因此每个IO请求
        // 的Descriptor链表，确实只有一个Descriptor，因此不需要对其进行遍历
        // （一个IO请求，对应了Linux内核中的一个散列表，Linux faascale使用了sg_init_one来初始化，所以其散列表中只有一个Descriptor）
        while let Some(head) = queue.pop(mem) {
            let len = head.len as usize; // 获取该Descriptor的数据区的大小，数据区存放的是guest返回的PFN
            let max_len = MAX_BLOCKS_IN_DESC * SIZE_OF_BLOCK_INFO; // 每个Descriptor最多存放256个PFN，也即1MB

            // head的数据区就是内核传输过来的pfns数组，因此其数据区的长度一定是整除SIZE_OF_U32的
            // is_write_only 为真表明，这个descriptors对于Device是write_only,而对于driver是read_only，显然在这里，应该对于firecracker应该是只读的
            if !head.is_write_only() && len % SIZE_OF_BLOCK_INFO == 0 { //
                // Check descriptor pfn count.
                // head的长度肯定不能超过最大的长度限制，即其最多存放256个pfn
                if len > max_len {
                    error!(
                            "populate descriptor has bogus page count {} > {}, skipping.",
                            len / SIZE_OF_BLOCK_INFO,
                            MAX_BLOCKS_IN_DESC
                        );

                    // Skip descriptor.
                    continue;
                }

                // This is safe, `len` was validated above.
                // 循环的遍历出Descriptor的数据区中所有的pfn
                for index in (0..len).step_by(SIZE_OF_BLOCK_INFO) {
                    // head.addr 是数据区的首地址，加上index后，就是每个fpn的地址，整个地址是虚拟机的物理地址
                    let addr = head
                        .addr
                        .checked_add(index as u64)
                        .ok_or(FaascaleMemError::MalformedDescriptor)?;

                    // 通过mem.read_obj，将pfn读出来
                    let block = mem
                        .read_obj::<[u32; 2]>(addr)
                        .map_err(|_| FaascaleMemError::MalformedDescriptor)?;

                    let guest_addr =
                        GuestAddress(u64::from(block[0]) << VIRTIO_FAASCALE_MEM_PFN_SHIFT);
                    let range = (guest_addr, u64::from(block[1]) << VIRTIO_FAASCALE_MEM_PFN_SHIFT);

                    match queue_index {
                        POPULATE_INDEX =>{
                            debug!("KINGDO: Populate Block: start_pfn={}, size={}",block[0],block[1]);
                            if let Err(err) = populate_range(
                                mem,
                                range,
                                self.restored,
                                self.pre_alloc_mem,
                                self.pre_tdp_fault,
                            ) {
                                error!("Error populating memory range: {:?}", err);
                            }
                        },
                        DEPOPULATE_INDEX =>{
                            debug!("KINGDO: Remove Block: start_pfn={}, size={}",block[0],block[1]);
                            if let Err(err) = remove_range(
                                mem,
                                range,
                                self.restored,
                            ) {
                                error!("Error removing memory range: {:?}", err);
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Acknowledge the receipt of the descriptor.
            // 0 is number of bytes the device has written to memory.
            // 告诉guest，我们已经读取完成了一个IO请求，其可以将指定的descriptor给释放掉。
            queue
                .add_used(mem, head.index, 0)
                .map_err(FaascaleMemError::Queue)?;
            needs_interrupt = true;
        }

        // 告诉虚拟机，我们已经完成了对一次IO请求，执行该函数后会触发Linux内核中vqueue的callbacks，
        if needs_interrupt {
            self.signal_used_queue()?;
        }

        Ok(())
    }

    pub(crate) fn process_stats_queue(&mut self) -> Result<(), FaascaleMemError> {
        // This is safe since we checked in the event handler that the device is activated.
        let mem = self.device_state.mem().unwrap();
        METRICS.faascale_mem.stats_updates_count.inc();

        while let Some(head) = self.queues[FAASCALE_STATS_INDEX].pop(mem) {
            if let Some(prev_stats_desc) = self.stats_desc_index {
                // We shouldn't ever have an extra buffer if the driver follows
                // the protocol, but return it if we find one.
                error!("faascale-mem: driver is not compliant, more than one stats buffer received");
                self.queues[FAASCALE_STATS_INDEX]
                    .add_used(mem, prev_stats_desc, 0)
                    .map_err(FaascaleMemError::Queue)?;
            }
            for index in (0..head.len).step_by(SIZE_OF_STAT) {
                // Read the address at position `index`. The only case
                // in which this fails is if there is overflow,
                // in which case this descriptor is malformed,
                // so we ignore the rest of it.
                let addr = head
                    .addr
                    .checked_add(u64::from(index))
                    .ok_or(FaascaleMemError::MalformedDescriptor)?;
                let stat = mem
                    .read_obj::<FaascaleMemStat>(addr)
                    .map_err(|_| FaascaleMemError::MalformedDescriptor)?;
                self.latest_stats.update_with_stat(&stat).map_err(|_| {
                    METRICS.faascale_mem.stats_update_fails.inc();
                    FaascaleMemError::MalformedPayload
                })?;
            }

            self.stats_desc_index = Some(head.index);
        }

        Ok(())
    }

    // 周期性的告诉guest，获取的states信息
    fn trigger_stats_update(&mut self) -> Result<(), FaascaleMemError> {
        // This is safe since we checked in the event handler that the device is activated.
        let mem = self.device_state.mem().unwrap();

        // The communication is driven by the device by using the buffer
        // and sending a used buffer notification
        if let Some(index) = self.stats_desc_index.take() {
            self.queues[FAASCALE_STATS_INDEX]
                .add_used(mem, index, 0)
                .map_err(FaascaleMemError::Queue)?;
            self.signal_used_queue()
        } else {
            error!("Failed to update faascale_mem stats, missing descriptor.");
            Ok(())
        }
    }

    pub(crate) fn signal_used_queue(&self) -> Result<(), FaascaleMemError> {
        self.irq_trigger.trigger_irq(IrqType::Vring).map_err(|err| {
            METRICS.faascale_mem.event_fails.inc();
            FaascaleMemError::InterruptError(err)
        })
    }

    /// Process device virtio queue(s).
    pub fn process_virtio_queues(&mut self) {
        let _ = self.process_populate_queue(POPULATE_INDEX);
        let _ = self.process_populate_queue(DEPOPULATE_INDEX);
    }

    pub fn id(&self) -> &str {
        FAASCALE_MEM_DEV_ID
    }

    // 当用户改变stats_polling_interval的配置时，会由src/vmm/src/lib.rs中的update_balloon_stats_config函数调用该函数
    pub fn update_stats_polling_interval(&mut self, interval_s: u16) -> Result<(), FaascaleMemError> {
        if self.stats_polling_interval_s == interval_s {
            return Ok(());
        }

        if self.stats_polling_interval_s == 0 || interval_s == 0 {
            return Err(FaascaleMemError::StatisticsStateChange);
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

    pub fn stats_polling_interval_s(&self) -> u16 {
        self.stats_polling_interval_s
    }

    pub fn pre_alloc_mem(&self) -> bool {
        self.pre_alloc_mem
    }

    pub fn pre_tdp_fault(&self) -> bool {
        self.pre_tdp_fault
    }


    pub fn latest_stats(&mut self) -> Option<&FaascaleMemStats> {
        if self.stats_enabled() {
            Some(&self.latest_stats)
        } else {
            None
        }
    }

    pub fn config(&self) -> FaascaleMemConfig {
        FaascaleMemConfig {
            stats_polling_interval_s: self.stats_polling_interval_s(),
            pre_alloc_mem: self.pre_alloc_mem(),
            pre_tdp_fault: self.pre_tdp_fault(),
        }
    }

    pub(crate) fn stats_enabled(&self) -> bool {
        self.stats_polling_interval_s > 0
    }

    pub(crate) fn set_stats_desc_index(&mut self, stats_desc_index: Option<u16>) {
        self.stats_desc_index = stats_desc_index;
    }
}

impl VirtioDevice for FaascaleMem {
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
        TYPE_FAASCALE_MEM
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
            error!("FaascaleMem: Cannot write to activate_evt");
            METRICS.faascale_mem.activate_fails.inc();
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