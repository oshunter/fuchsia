// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Module for IP fragmented packet reassembly support.
//!
//! `reassembly` is a utility to support reassembly of fragmented IP packets.
//! Fragmented packets are associated by a combination of the packets' source
//! address, destination address and identification value. When a potentially
//! fragmented packet is received, this utility will check to see if the packet
//! is in fact fragmented or not. If it isn't fragmented, it will be returned as
//! is without any modification. If it is fragmented, this utility will capture
//! its body and store it in a cache while waiting for all the fragments for a
//! packet to arrive. The header information from a fragment with offset set to
//! 0 will also be kept to add to the final, reassembled packet. Once this utility
//! has received all the fragments for a combination of source address, destination
//! address and identification value, the implementer will need to allocate a
//! buffer of sufficient size to reassemble the final packet into and pass it to
//! this utility. This utility will then attempt to reassemble and parse the
//! packet, which will be returned to the caller. The caller should then handle
//! the returned packet as a normal IP packet. Note, there is a timer from
//! receipt of the first fragment to reassembly of the final packet. See
//! [`REASSEMBLY_TIMEOUT_SECONDS`].
//!
//! Note, this utility does not support reassembly of jumbogram packets. According
//! to the IPv6 Jumbogram RFC (RFC 2675), the jumbogram payload option is relevent
//! only for nodes that may be attached to links with a link MTU greater than
//! 65575 bytes. Note, the maximum size of a non-jumbogram IPv6 packet is also
//! 65575 (as the payload length field for IP packets is 16 bits + 40 byte IPv6
//! header). If a link supports an MTU greater than the maximum size of a
//! non-jumbogram packet, the packet should not be fragmented.

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashMap};
use std::convert::TryFrom;
use std::time::Duration;

use byteorder::{ByteOrder, NetworkEndian};
use internet_checksum::Checksum;
use net_types::ip::{Ip, IpAddress};
use packet::{BufferViewMut, ParsablePacket};
use specialize_ip_macro::specialize_ip;
use zerocopy::{ByteSlice, ByteSliceMut};

use crate::context::{StateContext, TimerContext};
use crate::ip::{IpExtByteSlice, IpPacket};
use crate::wire::ipv4::{
    IPV4_CHECKSUM_BYTE_RANGE, IPV4_FRAGMENT_DATA_BYTE_RANGE, IPV4_TOTAL_LENGTH_BYTE_RANGE,
};
use crate::wire::ipv6::{IPV6_FIXED_HDR_LEN, IPV6_PAYLOAD_LEN_BYTE_RANGE};

/// The maximum amount of time from receipt of the first fragment to reassembly of
/// a packet. Note, "first fragment" does not mean a fragment with offset 0; it means
/// the first fragment packet we receive with a new combination of source address,
/// destination address and fragment identification value.
const REASSEMBLY_TIMEOUT_SECONDS: u64 = 60;

/// Number of bytes per fragment block for IPv4 and IPv6.
///
/// IPv4 outlines the fragment block size in RFC 791 section 3.1, under the fragment
/// offset field's description: "The fragment offset is measured in units of 8 octets
/// (64 bits)".
///
/// IPv6 outlines the fragment block size in RFC 8200 section 4.5, under the fragment
/// offset field's description: "The offset, in 8-octet units, of the data following
/// this header".
const FRAGMENT_BLOCK_SIZE: u8 = 8;

/// Maximum number of fragment blocks an IPv4 or IPv6 packet can have.
///
/// We use this value because both IPv4 fixed header's fragment offset field and
/// IPv6 fragment extension header's fragment offset field are 13 bits wide.
const MAX_FRAGMENT_BLOCKS: u16 = 8191;

/// The execution context for the fragment cache.
pub(crate) trait FragmentContext<I: Ip>:
    TimerContext<FragmentCacheKey<I::Addr>> + StateContext<(), IpLayerFragmentCache<I>>
{
}

impl<
        I: Ip,
        C: TimerContext<FragmentCacheKey<I::Addr>> + StateContext<(), IpLayerFragmentCache<I>>,
    > FragmentContext<I> for C
{
}

/// Handles reassembly timers.
///
/// See [`IpLayerFragmentCache::handle_reassembly_timer`].
pub(crate) fn handle_reassembly_timer<I: Ip, C: FragmentContext<I>>(
    ctx: &mut C,
    key: FragmentCacheKey<I::Addr>,
) {
    ctx.get_state_mut(()).handle_reassembly_timer(key);
}

/// Trait that must be implemented by any packet type that is fragmentable.
pub(crate) trait FragmentablePacket {
    /// Return fragment identifier data.
    ///
    /// Returns the fragment identification, offset and more flag as
    /// `(a, b, c)` where `a` is the fragment identification value,
    /// `b` is the fragment offset and `c` is the more flag.
    ///
    /// # Panics
    ///
    /// Panics if the packet has no fragment data.
    fn fragment_data(&self) -> (u32, u16, bool);
}

/// Possible return values for [`IpLayerFragmentCache::process_fragment`].
#[derive(Debug)]
pub(crate) enum FragmentProcessingState<B: ByteSlice, I: Ip> {
    /// The provided packet is not fragmented so no processing is required.
    /// The packet is returned with this value without any modification.
    NotNeeded(<I as IpExtByteSlice<B>>::Packet),

    /// The provided packet is fragmented but it is malformed.
    ///
    /// Possible reasons for being malformed are:
    ///  1) Body is not a multiple of `FRAGMENT_BLOCK_SIZE` and  it is not the
    ///     last fragment (last fragment of a packet, not last fragment received
    ///     for a packet).
    ///  2) Overlaps with an existing fragment. This is explicitly not allowed for
    ///     IPv6 as per RFC 8200 section 4.5 (more details in RFC 5722). We choose
    ///     the same behaviour for IPv4 for the same reasons.
    ///  3) Packet's fragment offset + # of fragment blocks > `MAX_FRAGMENT_BLOCKS`.
    // TODO(ghanan): Investigate whether disallowing overlapping fragments for IPv4
    //               cause issues interoperating with hosts that produce overlapping
    //               fragments.
    InvalidFragment,

    /// Successfully proccessed the provided fragment. We are still waiting on
    /// more fragments for a packet to arrive before being ready to reassemble the
    /// packet.
    NeedMoreFragments,

    /// Successfully processed the provided fragment. We now have all the fragments
    /// we need to reassemble the packet. The caller must create a buffer with capacity
    /// for at least `packet_len` bytes and provide the buffer and `key` to
    /// `reassemble_packet`.
    Ready { key: FragmentCacheKey<I::Addr>, packet_len: usize },
}

/// Possible errors when attempting to reassemble a packet.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FragmentReassemblyError {
    /// At least one fragment for a packet has not arrived.
    MissingFragments,

    /// A `FragmentCacheKey` is not associated with any packet. This could be
    /// because either no fragment has yet arrived for a packet associated with
    /// a `FragmentCacheKey` or some fragments did arrive, but the reassembly
    /// timer expired and got discarded.
    InvalidKey,

    /// Packet parsing error.
    PacketParsingError,
}

/// Fragment Cache Key.
///
/// Composed of the original packet's source address, destination address,
/// and fragment id.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct FragmentCacheKey<A: IpAddress>(A, A, u32);

impl<A: IpAddress> FragmentCacheKey<A> {
    fn new(src_ip: A, dst_ip: A, fragment_id: u32) -> Self {
        FragmentCacheKey(src_ip, dst_ip, fragment_id)
    }
}

/// Data required for fragmented packet reassembly.
#[derive(Debug)]
struct FragmentCacheData {
    /// List of non-overlapping inclusive ranges of fragment blocks required before
    /// being ready to reassemble a packet.
    ///
    /// When creating a new instance of `FragmentCacheData`, we will set `missing_blocks`
    /// to a list with a single element representing all blocks, (0, MAX_VALUE).
    /// In this case, MAX_VALUE will be set to `std::u16::MAX`.
    // TODO(ghanan): Consider a different data structure? With the BTreeSet, searches will be
    //               O(n) and inserts/removals may be O(log(n)). If we use a linked list,
    //               searches will be O(n) but inserts/removals will be O(1).
    //               For now, a `BTreeSet` is used since Rust provides one and it does the job of
    //               keeping the list of gaps in increasing order when searching, inserting and
    //               removing. How many fragments for a packet will we get in practice though?
    missing_blocks: BTreeSet<(u16, u16)>,

    /// Received fragment blocks.
    ///
    /// We use a binary heap for help when reassembling packets. When we reassemble
    /// packets, we will want to fill up a new buffer with all the body fragments.
    /// The easiest way to do this is in order, from the fragment with offset 0 to
    /// the fragment with the highest offset. Since we only need to enforce the order
    /// when reassembling, we use a min-heap so we have a defined order (increasing
    /// fragment offset values) when popping. `BinaryHeap` is technically a max-heap,
    /// but we use the negative of the offset values as the key for the heap. See
    /// [`PacketBodyFragment::new`].
    body_fragments: BinaryHeap<PacketBodyFragment>,

    /// The header data for the reassembled packet.
    ///
    /// The header of the fragment packet with offset 0 will be used as the header
    /// for the final, reassembled packet.
    header: Option<Vec<u8>>,

    /// Total number of bytes in the reassembled packet.
    ///
    /// This is used so that we don't have to iterated through `body_fragments` and
    /// sum the partial body sizes to calculate the reassembled packet's size.
    total_size: usize,
}

impl FragmentCacheData {
    /// Create a new `FragmentCacheData` with all fragments marked as missing.
    fn new() -> Self {
        let mut ret = FragmentCacheData {
            missing_blocks: BTreeSet::new(),
            body_fragments: BinaryHeap::new(),
            header: None,
            total_size: 0,
        };
        ret.missing_blocks.insert((0, std::u16::MAX));
        ret
    }
}

/// Structure to keep track of all fragments for a specific IP version, `I`.
///
/// The key for our hash map is a `FragmentCacheKey` with the associated data
/// a `FragmentCacheData`.
///
/// See [`FragmentCacheKey`] and [`FragmentCacheData`].
type FragmentCache<A> = HashMap<FragmentCacheKey<A>, FragmentCacheData>;

/// Type to process fragments and handle reassembly.
///
/// To keep track of partial fragments, we use a hash table. The key will be
/// composed of the (remote) source address, (local) destination address and
/// 32-bit identifier of a packet.
#[derive(Debug)]
pub(crate) struct IpLayerFragmentCache<I: Ip> {
    cache: FragmentCache<I::Addr>,
}

impl<I: Ip> IpLayerFragmentCache<I> {
    pub(crate) fn new() -> Self {
        IpLayerFragmentCache { cache: FragmentCache::new() }
    }

    /// Handle a reassembly timer.
    ///
    /// Removes reassembly data associated with a given `FragmentCacheKey`,
    /// `key`.
    fn handle_reassembly_timer(&mut self, key: FragmentCacheKey<I::Addr>) {
        // If a timer fired, the `key` must still exist in our fragment cache.
        assert!(self.cache.remove(&key).is_some());
    }
}

/// Attempts to process a packet fragment.
///
/// # Panics
///
/// Panics if the packet has no fragment data.
pub(crate) fn process_fragment<I: Ip, C: FragmentContext<I>, B: ByteSlice>(
    ctx: &mut C,
    packet: <I as IpExtByteSlice<B>>::Packet,
) -> FragmentProcessingState<B, I>
where
    <I as IpExtByteSlice<B>>::Packet: FragmentablePacket,
{
    // Get the fragment data.
    let (id, offset, m_flag) = packet.fragment_data();

    // Check if `packet` is actually fragmented. We know it is not fragmented if
    // the fragment offset is 0 (contains first fragment) and we have no more
    // fragments. This means the first fragment is the only fragment, implying
    // we have a full packet.
    if offset == 0 && !m_flag {
        return FragmentProcessingState::NotNeeded(packet);
    }

    // Make sure packet's body isn't empty. Since at this point we know that the
    // packet is definitely fragmented (`offset` is not 0 or `m_flag` is
    // `true`), we simply let the caller know we need more fragments. This
    // should never happen, but just in case :).
    if packet.body().is_empty() {
        return FragmentProcessingState::NeedMoreFragments;
    }

    // Make sure body is a multiple of `FRAGMENT_BLOCK_SIZE` bytes, or `packet`
    // contains the last fragment block which is allowed to be less than
    // `FRAGMENT_BLOCK_SIZE` bytes.
    if m_flag && (packet.body().len() % (FRAGMENT_BLOCK_SIZE as usize) != 0) {
        return FragmentProcessingState::InvalidFragment;
    }

    // Key used to find this connection's fragment cache data.
    let key = FragmentCacheKey::new(packet.src_ip(), packet.dst_ip(), id);

    // Get (or create) the fragment cache data.
    let fragment_data = get_or_create(ctx, &key);

    // The number of fragment blocks `packet` contains.
    //
    // Note, we are calculating the ceiling of an integer division. Essentially:
    //     ceil(packet.body.len() / FRAGMENT_BLOCK_SIZE)
    //
    // We need to calculate the ceiling of the division because the final
    // fragment block for a reassembled packet is allowed to contain less than
    // `FRAGMENT_BLOCK_SIZE` bytes.
    //
    // We know `packet.body().len() - 1` will never be less than 0 because we
    // already made sure that `packet`'s body is not empty, and it is impossible
    // to have a negative body size.
    let num_fragment_blocks = 1 + ((packet.body().len() - 1) / (FRAGMENT_BLOCK_SIZE as usize));
    assert!(num_fragment_blocks > 0);

    // The range of fragment blocks `packet` contains.
    //
    // The maximum number of fragment blocks a reassembled packet is allowed to
    // contain is `MAX_FRAGMENT_BLOCKS` so we make sure that the fragment we
    // received does not violate this.
    let fragment_blocks_range =
        if let Ok(offset_end) = u16::try_from((offset as usize) + num_fragment_blocks - 1) {
            if offset_end <= MAX_FRAGMENT_BLOCKS {
                (offset, offset_end)
            } else {
                return FragmentProcessingState::InvalidFragment;
            }
        } else {
            return FragmentProcessingState::InvalidFragment;
        };

    // Find the gap where `packet` belongs.
    let found_gap = match find_gap(&fragment_data.missing_blocks, fragment_blocks_range) {
        // We did not find a potential gap `packet` fits in so some of the
        // fragment blocks in `packet` overlaps with fragment blocks we already
        // received.
        None => {
            // Drop all reassembly data as per RFC 8200 section 4.5 (IPv6). See
            // RFC 5722 for more information.
            //
            // IPv4 (RFC 791) does not specify what to do for overlapped
            // fragments. RFC 1858 section 4.2 outlines a way to prevent an
            // overlapping fragment attack for IPv4, but this is primarily for
            // IP filtering since "no standard requires that an overlap-safe
            // reassemble algorithm be used" on hosts. In practice,
            // non-malicious nodes should not intentionally send data for the
            // same fragment block multiple times, so we will do the same thing
            // as IPv6 in this case.
            //
            // TODO(ghanan): Check to see if the fragment block's data is
            //               identical to already received data before dropping
            //               the reassembly data as packets may be duplicated in
            //               the network. Duplicate packets which are also
            //               fragmented are probably rare, so we should first
            //               determine if it is even worthwhile to do this check
            //               first. Note, we can choose to simply not do this
            //               check as RFC 8200 section 4.5 mentions an
            //               implementation *may choose* to do this check. It
            //               does not say we MUST, so we would not be violating
            //               the RFC if we don't check for this case and just
            //               drop the packet.
            assert!(ctx.get_state_mut(()).cache.remove(&key).is_some());
            assert!(ctx.cancel_timer(key).is_some());

            return FragmentProcessingState::InvalidFragment;
        }
        Some(f) => f,
    };

    // Remove `found_gap` since the gap as it exists will no longer be valid.
    fragment_data.missing_blocks.remove(&found_gap);

    // If the received fragment blocks start after the beginning of `found_gap`,
    // create a new gap between the beginning of `found_gap` and the first
    // fragment block contained in `packet`.
    //
    // Example:
    //   `packet` w/ fragments [4, 7]
    //                 |-----|-----|-----|-----|
    //                    4     5     6     7
    //
    //   `found_gap` w/ fragments [X, 7] where 0 <= X < 4
    //     |-----| ... |-----|-----|-----|-----|
    //        X    ...    4     5     6     7
    //
    //   Here we can see that with a `found_gap` of [2, 7], `packet` covers [4,
    //   7] but we are still missing [X, 3] so we create a new gap of [X, 3].
    if found_gap.0 < fragment_blocks_range.0 {
        fragment_data.missing_blocks.insert((found_gap.0, fragment_blocks_range.0 - 1));
    }

    // If the received fragment blocks end before the end of `found_gap` and we
    // expect more fragments, create a new gap between the last fragment block
    // contained in `packet` and the end of `found_gap`.
    //
    // Example 1:
    //   `packet` w/ fragments [4, 7] & m_flag = true
    //     |-----|-----|-----|-----|
    //        4     5     6     7
    //
    //   `found_gap` w/ fragments [4, Y] where 7 < Y <= `MAX_FRAGMENT_BLOCKS`.
    //     |-----|-----|-----|-----| ... |-----|
    //        4     5     6     7    ...    Y
    //
    //   Here we can see that with a `found_gap` of [4, Y], `packet` covers [4,
    //   7] but we still expect more fragment blocks after the blocks in
    //   `packet` (as noted by `m_flag`) so we are still missing [8, Y] so we
    //   create a new gap of [8, Y].
    //
    // Example 2:
    //   `packet` w/ fragments [4, 7] & m_flag = false
    //     |-----|-----|-----|-----|
    //        4     5     6     7
    //
    //   `found_gap` w/ fragments [4, Y] where MAX = `MAX_FRAGMENT_BLOCKS`.
    //     |-----|-----|-----|-----| ... |-----|
    //        4     5     6     7    ...   MAX
    //
    //   Here we can see that with a `found_gap` of [4, MAX], `packet` covers
    //   [4, 7] and we don't expect more fragment blocks after the blocks in
    //   `packet` (as noted by `m_flag`) so we dont create a new gap. Note, if
    //   we encounter a `packet` where `m_flag` is false, `found_gap`'s end
    //   value must be MAX because we should only ever not create a new gap
    //   where the end is MAX when we are processing a packet with the last
    //   fragment block.
    if (found_gap.1 > fragment_blocks_range.1) && m_flag {
        fragment_data.missing_blocks.insert((fragment_blocks_range.1 + 1, found_gap.1));
    } else {
        // Make sure that if we are not adding a fragment after the packet, it
        // is because `packet` goes up to the `found_gap`'s end boundary, or
        // this is the last fragment. If it is the last fragment for a packet,
        // we make sure that `found_gap`'s end value is `std::u16::MAX`.
        assert!(found_gap.1 == fragment_blocks_range.1 || !m_flag && found_gap.1 == std::u16::MAX);
    }

    // Get header buffer from `packet` if its fragment offset equals to 0.
    if offset == 0 {
        assert!(fragment_data.header.is_none());
        let header = get_header::<B, I>(&packet);
        fragment_data.total_size += header.len();
        fragment_data.header = Some(header);
    }

    // Add our `packet`'s body to the store of body fragments.
    let mut body = Vec::with_capacity(packet.body().len());
    body.extend_from_slice(packet.body());
    fragment_data.total_size += body.len();
    fragment_data.body_fragments.push(PacketBodyFragment::new(offset, body));

    // If we still have missing fragments, let the caller know that we are still
    // waiting on some fragments. Otherwise, we let them know we are ready to
    // reassemble and give them a key and the final packet length so they can
    // allocate a sufficient buffer and call `reassemble_packet`.
    if fragment_data.missing_blocks.is_empty() {
        FragmentProcessingState::Ready { key, packet_len: fragment_data.total_size }
    } else {
        FragmentProcessingState::NeedMoreFragments
    }
}

/// Attempts to reassemble a packet.
///
/// Attempts to reassemble a packet associated with a given `FragmentCacheKey`,
/// `key`, and cancels the timer to reset reassembly data. The caller is
/// expected to allocate a buffer of sufficient size (available from
/// `process_fragment` when it returns a `FragmentProcessingState::Ready` value)
/// and provide it to `reassemble_packet` as `buffer` where the packet will be
/// reassembled into.
///
/// # Panics
///
/// Panics if the provided `buffer` does not have enough capacity for the
/// reassembled packet. Also panics if a different `ctx` is passed to
/// `reassemble_packet` from the one passed to `process_fragment` when
/// processing a packet with a given `key` as `reassemble_packet` will fail to
/// cancel the reassembly timer.
pub(crate) fn reassemble_packet<
    I: Ip,
    C: FragmentContext<I>,
    B: ByteSliceMut,
    BV: BufferViewMut<B>,
>(
    ctx: &mut C,
    key: &FragmentCacheKey<I::Addr>,
    buffer: BV,
) -> Result<<I as IpExtByteSlice<B>>::Packet, FragmentReassemblyError> {
    // Get the fragment cache data.
    let fragment_data = match ctx.get_state_mut(()).cache.get_mut(key) {
        // Either there are no fragments for the given `key`, or we timed out
        // and removed all fragment data for `key`.
        None => return Err(FragmentReassemblyError::InvalidKey),
        Some(d) => d,
    };

    // Make sure we are not missing fragments.
    if !fragment_data.missing_blocks.is_empty() {
        return Err(FragmentReassemblyError::MissingFragments);
    }

    // If we are not missing fragments, we must have header data.
    assert!(fragment_data.header.is_some());

    // Cancel the reassembly timer now that we know we have all the data
    // required for reassembly and are attempting to do so.
    assert!(ctx.cancel_timer(*key).is_some());

    // Take the header and body fragments from the cache data and remove the
    // cache data associated with `key` since it will no longer be needed.
    let data = ctx.get_state_mut(()).cache.remove(key).unwrap();
    let (header, body_fragments) = (data.header.unwrap(), data.body_fragments);

    // Attempt to actually reassemble the packet.
    reassemble_packet_helper::<B, BV, I>(buffer, header, body_fragments)
}

/// Gets or creates a new entry in the cache for a given `key`.
///
/// When a new entry is created, a re-assembly timer is scheduled.
fn get_or_create<'a, I: Ip, C: FragmentContext<I>>(
    ctx: &'a mut C,
    key: &FragmentCacheKey<I::Addr>,
) -> &'a mut FragmentCacheData {
    if ctx.get_state(()).cache.contains_key(key) {
        ctx.get_state_mut(()).cache.get_mut(key).unwrap()
    } else {
        // We have no reassembly data yet so this fragment is the first one
        // associated with the given `key`. Create a new entry in the hash table
        // and schedule a timer to reset the entry after
        // `REASSEMBLY_TIMEOUT_SECONDS` seconds.
        ctx.get_state_mut(()).cache.insert(key.clone(), FragmentCacheData::new());
        ctx.schedule_timer(Duration::from_secs(REASSEMBLY_TIMEOUT_SECONDS), *key);
        ctx.get_state_mut(()).cache.get_mut(key).unwrap()
    }
}

/// Attempts to find a gap where `fragment_blocks_range` will fit in.
///
/// Returns a `Some(o)` if a valid gap is found where `o` is the gap's offset
/// range; otherwise, returns `None`. `fragment_blocks_range` is an inclusive
/// range of fragment block offsets. `missing_blocks` is a list of
/// non-overlapping inclusive ranges of fragment blocks still required before
/// being ready to reassemble a packet.
fn find_gap(
    missing_blocks: &BTreeSet<(u16, u16)>,
    fragment_blocks_range: (u16, u16),
) -> Option<(u16, u16)> {
    for potential_gap in missing_blocks.iter() {
        if fragment_blocks_range.1 < potential_gap.0 || fragment_blocks_range.0 > potential_gap.1 {
            // Either:
            // - Our packet's ending offset is less than the start of
            //   `potential_gap` so move on to the next gap. That is,
            //   `fragment_blocks_range` ends before `potential_gap`.
            // - Our packet's starting offset is more than `potential_gap`'s
            //   ending offset so move on to the next gap. That is,
            //   `fragment_blocks_range` starts after `potential_gap`.
            continue;
        }

        // Make sure that `fragment_blocks_range` belongs purely within
        // `potential_gap`.
        //
        // If `fragment_blocks_range` does not fit purely within
        // `potential_gap`, then at least one block in `fragment_blocks_range`
        // overlaps with an already received block. We should never receive
        // overlapping fragments from non-malicious nodes.
        if (fragment_blocks_range.0 < potential_gap.0)
            || (fragment_blocks_range.1 > potential_gap.1)
        {
            break;
        }

        // Found a gap where `fragment_blocks_range` fits in!
        return Some(*potential_gap);
    }

    // Unable to find a valid gap so return `None`.
    None
}

/// Attempts to reassemble a packet.
///
/// Given a header buffer (`header`), body fragments (`body_fragments`), and a
/// buffer where the packet will be reassembled into (`buffer`), reassemble and
/// return a packet.
#[specialize_ip]
fn reassemble_packet_helper<B: ByteSliceMut, BV: BufferViewMut<B>, I: Ip>(
    mut buffer: BV,
    header: Vec<u8>,
    mut body_fragments: BinaryHeap<PacketBodyFragment>,
) -> Result<<I as IpExtByteSlice<B>>::Packet, FragmentReassemblyError> {
    let bytes = buffer.as_mut();

    // First, copy over the header data.
    bytes[0..header.len()].copy_from_slice(&header[..]);
    let mut byte_count = header.len();

    // Next, copy over the body fragments in ascending fragment offset order.
    while !body_fragments.is_empty() {
        // We know the call to `unwrap` won't panic because we make sure that
        // `body_fragments` is not empty before each iteration of the loop. If
        // `body_fragments` is not empty, then the call to `pop` must return a
        // `Some(p)` where p is a body fragment.
        let PacketBodyFragment(offset, p) = body_fragments.pop().unwrap();
        bytes[byte_count..byte_count + p.len()].copy_from_slice(&p[..]);
        byte_count += p.len();
    }

    #[ipv4]
    {
        //
        // Fix up the IPv4 header
        //

        // Make sure that the packet length is not more than the maximum
        // possible IPv4 packet length.
        if byte_count > (std::u16::MAX as usize) {
            return Err(FragmentReassemblyError::PacketParsingError);
        }

        // Update the total length field.
        NetworkEndian::write_u16(&mut bytes[IPV4_TOTAL_LENGTH_BYTE_RANGE], byte_count as u16);

        // Zero out fragment related data since we will now have a reassembled
        // packet that does not need reassembly.
        NetworkEndian::write_u32(&mut bytes[IPV4_FRAGMENT_DATA_BYTE_RANGE], 0);

        // Update header checksum. The header checksum field is at bytes 10 and
        // 11 so do not include them in the checksum calculation.
        let mut c = Checksum::new();
        c.add_bytes(&bytes[..IPV4_CHECKSUM_BYTE_RANGE.start]);
        c.add_bytes(&bytes[IPV4_CHECKSUM_BYTE_RANGE.end..header.len()]);
        bytes[IPV4_CHECKSUM_BYTE_RANGE].copy_from_slice(&c.checksum()[..]);
    }

    #[ipv6]
    {
        //
        // Fix up the IPv6 header
        //

        // For IPv6, the payload length is the sum of the length of the
        // extension headers and the packet body. The header as it is stored
        // includes the IPv6 fixed header and all extension headers, so
        // `bytes_count` is the sum of the size of the fixed header, extension
        // headers and packet body. To calculate the payload length we subtract
        // the size of the fixed header from the total byte count of a
        // reassembled packet.
        let payload_length = byte_count - IPV6_FIXED_HDR_LEN;

        // Make sure that the payload length is not more than the maximum
        // possible IPv4 packet length.
        if payload_length > (std::u16::MAX as usize) {
            return Err(FragmentReassemblyError::PacketParsingError);
        }

        // Update the payload length field.
        NetworkEndian::write_u16(&mut bytes[IPV6_PAYLOAD_LEN_BYTE_RANGE], payload_length as u16);
    }

    // Parse the packet.
    match <<I as IpExtByteSlice<B>>::Packet as ParsablePacket<B, _>>::parse_mut(buffer, ()) {
        Ok(p) => Ok(p),
        _ => Err(FragmentReassemblyError::PacketParsingError),
    }
}

/// Get the header bytes for a packet.
#[specialize_ip]
fn get_header<B: ByteSlice, I: Ip>(packet: &<I as IpExtByteSlice<B>>::Packet) -> Vec<u8> {
    #[ipv4]
    {
        packet.copy_header_bytes_for_fragment()
    }

    #[ipv6]
    {
        // We are guaranteed not to panic here because we will only panic if
        // `packet` does not have a fragment extension header. We can only get
        // here if `packet` is a fragment packet, so we know that `packet` has a
        // fragment extension header.
        packet.copy_header_bytes_for_fragment()
    }
}

/// A fragment of a packet's body.
///
/// The first value is the fragment offset, and the second value is the body
/// data.
#[derive(Debug, PartialEq, Eq)]
struct PacketBodyFragment(i32, Vec<u8>);

impl PacketBodyFragment {
    /// Construct a new `PacketBodyFragment` to be stored in a `BinaryHeap`.
    fn new(offset: u16, data: Vec<u8>) -> Self {
        // We want a min heap but `BinaryHeap` is a max heap, so we multiple
        // `offset` with -1.
        PacketBodyFragment(-(i32::from(offset)), data)
    }
}

// Ordering of a `PacketBodyFragment` is only dependant on the fragment offset
// value (first element in the tuple).
impl PartialOrd for PacketBodyFragment {
    fn partial_cmp(&self, other: &PacketBodyFragment) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PacketBodyFragment {
    fn cmp(&self, other: &PacketBodyFragment) -> Ordering {
        self.0.cmp(&other.0)
    }
}

#[cfg(test)]
mod tests {
    use net_types::ip::{IpAddress, Ipv4, Ipv6};
    use net_types::Witness;
    use packet::{Buf, ParseBuffer, Serializer};
    use specialize_ip_macro::{ip_test, specialize_ip};

    use super::*;
    use crate::ip::{IpProto, Ipv6ExtHdrType};
    use crate::testutil::{
        get_dummy_config, run_for, trigger_next_timer, DummyEventDispatcher,
        DummyEventDispatcherBuilder, DUMMY_CONFIG_V4, DUMMY_CONFIG_V6,
    };
    use crate::wire::ipv4::{Ipv4Packet, Ipv4PacketBuilder};
    use crate::wire::ipv6::{Ipv6Packet, Ipv6PacketBuilder};
    use crate::{Context, EventDispatcher};

    macro_rules! assert_frag_proc_state_need_more {
        ($lhs:expr) => {{
            let lhs_val = $lhs;
            match lhs_val {
                FragmentProcessingState::NeedMoreFragments => lhs_val,
                _ => panic!("{:?} is not `NeedMoreFragments`", lhs_val),
            }
        }};
    }

    macro_rules! assert_frag_proc_state_invalid {
        ($lhs:expr) => {{
            let lhs_val = $lhs;
            match lhs_val {
                FragmentProcessingState::InvalidFragment => lhs_val,
                _ => panic!("{:?} is not `InvalidFragment`", lhs_val),
            }
        }};
    }

    macro_rules! assert_frag_proc_state_ready {
        ($lhs:expr, $src_ip:expr, $dst_ip:expr, $fragment_id:expr, $packet_len:expr) => {{
            let lhs_val = $lhs;
            match lhs_val {
                FragmentProcessingState::Ready { key, packet_len } => {
                    if key == FragmentCacheKey::new($src_ip, $dst_ip, $fragment_id as u32)
                        && packet_len == $packet_len
                    {
                        (key, packet_len)
                    } else {
                        panic!("Invalid key or packet_len values");
                    }
                }
                _ => panic!("{:?} is not `Ready`", lhs_val),
            }
        }};
    }

    /// The result `process_ipv4_fragment` or `process_ipv6_fragment` should
    /// expect after processing a fragment.
    #[derive(PartialEq)]
    enum ExpectedResult {
        /// After processing a packet fragment, we should be ready to reassemble
        /// the packet.
        Ready,

        /// After processing a packet fragment, we should successfully
        /// reassemble a packet.
        ReadyReassemble,

        /// After processing a packet fragment, we need more packet fragments
        /// before being ready to reassemble the packet.
        NeedMore,

        /// The packet fragment is invalid.
        Invalid,
    }

    /// Get an IPv4 packet builder.
    fn get_ipv4_builder() -> Ipv4PacketBuilder {
        Ipv4PacketBuilder::new(
            DUMMY_CONFIG_V4.remote_ip,
            DUMMY_CONFIG_V4.local_ip,
            10,
            IpProto::Tcp,
        )
    }

    /// Get an IPv6 packet builder.
    fn get_ipv6_builder() -> Ipv6PacketBuilder {
        Ipv6PacketBuilder::new(
            DUMMY_CONFIG_V6.remote_ip,
            DUMMY_CONFIG_V6.local_ip,
            10,
            IpProto::Tcp,
        )
    }

    /// Process an IP fragment depending on the `Ip` `process_ip_fragment` is
    /// specialized with.
    ///
    /// See [`process_ipv4_fragment`] and [`process_ipv6_fragment`] which will
    /// be called when `process_ip_fragment` is specialized for `Ipv4` and
    /// `Ipv6`, respectively.
    #[specialize_ip]
    fn process_ip_fragment<I: Ip, D: EventDispatcher>(
        ctx: &mut Context<D>,
        fragment_id: u16,
        fragment_offset: u8,
        fragment_count: u8,
        expected_result: ExpectedResult,
    ) {
        #[ipv4]
        process_ipv4_fragment(ctx, fragment_id, fragment_offset, fragment_count, expected_result);

        #[ipv6]
        process_ipv6_fragment(ctx, fragment_id, fragment_offset, fragment_count, expected_result);
    }

    /// Generate and process an IPv4 fragment packet.
    ///
    /// `fragment_offset` is the fragment offset. `fragment_count` is the number
    /// of fragments for a packet. The generated packet will have body of size
    /// `FRAGMENT_BLOCK_SIZE` bytes.
    ///
    /// `process_ipv4_fragment` will expect the result specified by
    /// `expected_result` when processing the result.
    fn process_ipv4_fragment<D: EventDispatcher>(
        ctx: &mut Context<D>,
        fragment_id: u16,
        fragment_offset: u8,
        fragment_count: u8,
        expected_result: ExpectedResult,
    ) {
        assert!(fragment_offset < fragment_count);

        let m_flag = fragment_offset < (fragment_count - 1);
        // Use fragment_id to offset the body data values so not all fragment
        // packets with the same `fragment_offset` will have the same data.
        let body_offset = fragment_id as u8;

        let mut builder = get_ipv4_builder();
        builder.id(fragment_id);
        builder.fragment_offset(fragment_offset as u16);
        builder.mf_flag(m_flag);
        let mut body: Vec<u8> = Vec::new();
        body.extend(
            body_offset + fragment_offset * FRAGMENT_BLOCK_SIZE
                ..body_offset + fragment_offset * FRAGMENT_BLOCK_SIZE + FRAGMENT_BLOCK_SIZE,
        );

        let mut buffer = Buf::new(body, ..).encapsulate(builder).serialize_vec_outer().unwrap();
        let packet = buffer.parse::<Ipv4Packet<_>>().unwrap();

        match expected_result {
            ExpectedResult::Ready | ExpectedResult::ReadyReassemble => {
                // We add 20 to the expected packet length because of the IPv4 header.
                let (key, packet_len) = assert_frag_proc_state_ready!(
                    process_fragment::<Ipv4, _, &[u8]>(ctx, packet),
                    DUMMY_CONFIG_V4.remote_ip.get(),
                    DUMMY_CONFIG_V4.local_ip.get(),
                    fragment_id,
                    (FRAGMENT_BLOCK_SIZE as usize) * (fragment_count as usize) + 20
                );

                if expected_result == ExpectedResult::ReadyReassemble {
                    let mut buffer: Vec<u8> = vec![0; packet_len];
                    let mut buffer = &mut buffer[..];
                    let packet =
                        reassemble_packet::<Ipv4, _, &mut [u8], _>(ctx, &key, &mut buffer).unwrap();
                    let mut expected_body: Vec<u8> = Vec::new();
                    expected_body
                        .extend(body_offset..body_offset + fragment_count * FRAGMENT_BLOCK_SIZE);
                    assert_eq!(packet.body(), &expected_body[..]);
                }
            }
            ExpectedResult::NeedMore => {
                assert_frag_proc_state_need_more!(process_fragment::<Ipv4, _, &[u8]>(ctx, packet));
            }
            ExpectedResult::Invalid => {
                assert_frag_proc_state_invalid!(process_fragment::<Ipv4, _, &[u8]>(ctx, packet));
            }
        }
    }

    /// Generate and process an IPv6 fragment packet.
    ///
    /// `fragment_offset` is the fragment offset. `fragment_count` is the number
    /// of fragments for a packet. The generated packet will have body of size
    /// `FRAGMENT_BLOCK_SIZE` bytes.
    ///
    /// `process_ipv6_fragment` will expect the result specified by
    /// `expected_result` when processing the result.
    fn process_ipv6_fragment<D: EventDispatcher>(
        ctx: &mut Context<D>,
        fragment_id: u16,
        fragment_offset: u8,
        fragment_count: u8,
        expected_result: ExpectedResult,
    ) {
        assert!(fragment_offset < fragment_count);

        let m_flag = fragment_offset < (fragment_count - 1);
        // Use fragment_id to offset the body data values so not all fragment
        // packets with the same `fragment_offset` will have the same data.
        let body_offset = fragment_id as u8;

        let mut bytes = vec![0; 48];
        bytes[..4].copy_from_slice(&[0x60, 0x20, 0x00, 0x77][..]);
        bytes[6] = Ipv6ExtHdrType::Fragment.into(); // Next Header
        bytes[7] = 64;
        bytes[8..24].copy_from_slice(DUMMY_CONFIG_V6.remote_ip.bytes());
        bytes[24..40].copy_from_slice(DUMMY_CONFIG_V6.local_ip.bytes());
        bytes[40] = IpProto::Tcp.into();
        bytes[42] = fragment_offset >> 5;
        bytes[43] = ((fragment_offset & 0x1F) << 3) | if m_flag { 1 } else { 0 };
        NetworkEndian::write_u32(&mut bytes[44..48], fragment_id as u32);
        bytes.extend(
            body_offset + fragment_offset * FRAGMENT_BLOCK_SIZE
                ..body_offset + fragment_offset * FRAGMENT_BLOCK_SIZE + FRAGMENT_BLOCK_SIZE,
        );
        let payload_len = (bytes.len() - 40) as u16;
        NetworkEndian::write_u16(&mut bytes[4..6], payload_len);
        let mut buf = Buf::new(bytes, ..);
        let packet = buf.parse::<Ipv6Packet<_>>().unwrap();

        match expected_result {
            ExpectedResult::Ready | ExpectedResult::ReadyReassemble => {
                // We add 20 to the expected packet length because of the IPv4 header.
                let (key, packet_len) = assert_frag_proc_state_ready!(
                    process_fragment::<Ipv6, _, &[u8]>(ctx, packet),
                    DUMMY_CONFIG_V6.remote_ip.get(),
                    DUMMY_CONFIG_V6.local_ip.get(),
                    fragment_id,
                    (FRAGMENT_BLOCK_SIZE as usize) * (fragment_count as usize) + 40
                );

                if expected_result == ExpectedResult::ReadyReassemble {
                    let mut buffer: Vec<u8> = vec![0; packet_len];
                    let mut buffer = &mut buffer[..];
                    let packet =
                        reassemble_packet::<Ipv6, _, &mut [u8], _>(ctx, &key, &mut buffer).unwrap();
                    let mut expected_body: Vec<u8> = Vec::new();
                    expected_body
                        .extend(body_offset..body_offset + fragment_count * FRAGMENT_BLOCK_SIZE);
                    assert_eq!(packet.body(), &expected_body[..]);
                }
            }
            ExpectedResult::NeedMore => {
                assert_frag_proc_state_need_more!(process_fragment::<Ipv6, _, &[u8]>(ctx, packet));
            }
            ExpectedResult::Invalid => {
                assert_frag_proc_state_invalid!(process_fragment::<Ipv6, _, &[u8]>(ctx, packet));
            }
        }
    }

    #[test]
    fn test_ipv4_reassembly_not_needed() {
        let mut ctx = DummyEventDispatcherBuilder::from_config(DUMMY_CONFIG_V4)
            .build::<DummyEventDispatcher>();

        //
        // Test that we don't attempt reassembly if the packet is not
        // fragmented.
        //

        let builder = get_ipv4_builder();
        let mut buffer =
            Buf::new(vec![1, 2, 3, 4, 5], ..).encapsulate(builder).serialize_vec_outer().unwrap();
        let packet = buffer.parse::<Ipv4Packet<_>>().unwrap();
        process_fragment::<Ipv4, _, &[u8]>(&mut ctx, packet);
    }

    #[test]
    #[should_panic]
    fn test_ipv6_reassembly_not_needed() {
        let mut ctx = DummyEventDispatcherBuilder::from_config(DUMMY_CONFIG_V6)
            .build::<DummyEventDispatcher>();

        //
        // Test that we panic if we call `fragment_data` on a packet that has no
        // fragment data.
        //

        let builder = get_ipv6_builder();
        let mut buffer =
            Buf::new(vec![1, 2, 3, 4, 5], ..).encapsulate(builder).serialize_vec_outer().unwrap();
        let packet = buffer.parse::<Ipv6Packet<_>>().unwrap();
        process_fragment::<Ipv6, _, &[u8]>(&mut ctx, packet);
    }

    #[ip_test]
    fn test_ip_reassembly<I: Ip>() {
        let mut ctx = DummyEventDispatcherBuilder::from_config(get_dummy_config::<I::Addr>())
            .build::<DummyEventDispatcher>();
        let fragment_id = 5;

        //
        // Test that we properly reassemble fragmented packets.
        //

        // Process fragment #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id, 0, 3, ExpectedResult::NeedMore);

        // Process fragment #1
        process_ip_fragment::<I, _>(&mut ctx, fragment_id, 1, 3, ExpectedResult::NeedMore);

        // Process fragment #2
        process_ip_fragment::<I, _>(&mut ctx, fragment_id, 2, 3, ExpectedResult::ReadyReassemble);
    }

    #[ip_test]
    fn test_ip_reassemble_with_missing_blocks<I: Ip>() {
        let dummy_config = get_dummy_config::<I::Addr>();
        let mut ctx = DummyEventDispatcherBuilder::from_config(dummy_config.clone())
            .build::<DummyEventDispatcher>();
        let fragment_id = 5;

        //
        // Test the error we get when we attempt to reassemble with missing
        // fragments.
        //

        // Process fragment #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id, 0, 3, ExpectedResult::NeedMore);

        // Process fragment #2
        process_ip_fragment::<I, _>(&mut ctx, fragment_id, 1, 3, ExpectedResult::NeedMore);

        let mut buffer: Vec<u8> = vec![0; 1];
        let mut buffer = &mut buffer[..];
        let key = FragmentCacheKey::new(
            dummy_config.remote_ip.get(),
            dummy_config.local_ip.get(),
            fragment_id as u32,
        );
        assert_eq!(
            reassemble_packet::<I, _, &mut [u8], _>(&mut ctx, &key, &mut buffer).unwrap_err(),
            FragmentReassemblyError::MissingFragments,
        );
    }

    #[ip_test]
    fn test_ip_reassemble_after_timer<I: Ip>() {
        let dummy_config = get_dummy_config::<I::Addr>();
        let mut ctx = DummyEventDispatcherBuilder::from_config(dummy_config.clone())
            .build::<DummyEventDispatcher>();
        let fragment_id = 5;

        // Make sure no timers in the dispatcher yet.
        assert_eq!(ctx.dispatcher.timer_events().count(), 0);

        //
        // Test that we properly reset fragment cache on timer.
        //

        // Process fragment #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id, 0, 3, ExpectedResult::NeedMore);
        // Make sure a timer got added.
        assert_eq!(ctx.dispatcher.timer_events().count(), 1);

        // Process fragment #1
        process_ip_fragment::<I, _>(&mut ctx, fragment_id, 1, 3, ExpectedResult::NeedMore);
        // Make sure no new timers got added or fired.
        assert_eq!(ctx.dispatcher.timer_events().count(), 1);

        // Process fragment #2
        process_ip_fragment::<I, _>(&mut ctx, fragment_id, 2, 3, ExpectedResult::Ready);
        // Make sure no new timers got added or fired.
        assert_eq!(ctx.dispatcher.timer_events().count(), 1);

        // Trigger the timer (simulate a timer for the fragmented packet)
        assert!(trigger_next_timer(&mut ctx));

        // Make sure no other times exist..
        assert_eq!(ctx.dispatcher.timer_events().count(), 0);

        // Attempt to reassemble the packet but get an error since the fragment
        // data would have been reset/cleared.
        let key = FragmentCacheKey::new(
            dummy_config.local_ip.get(),
            dummy_config.remote_ip.get(),
            fragment_id as u32,
        );
        let packet_len = 44;
        let mut buffer: Vec<u8> = vec![0; packet_len];
        let mut buffer = &mut buffer[..];
        assert_eq!(
            reassemble_packet::<I, _, &mut [u8], _>(&mut ctx, &key, &mut buffer).unwrap_err(),
            FragmentReassemblyError::InvalidKey,
        );
    }

    #[ip_test]
    fn test_ip_overlapping_single_fragment<I: Ip>() {
        let mut ctx = DummyEventDispatcherBuilder::from_config(get_dummy_config::<I::Addr>())
            .build::<DummyEventDispatcher>();
        let fragment_id = 5;

        //
        // Test that we error on overlapping/duplicate fragments.
        //

        // Process fragment #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id, 0, 3, ExpectedResult::NeedMore);

        // Process fragment #0 (overlaps original fragment #0 completely)
        process_ip_fragment::<I, _>(&mut ctx, fragment_id, 0, 3, ExpectedResult::Invalid);
    }

    #[test]
    fn test_ipv4_fragment_not_multiple_of_offset_unit() {
        let mut ctx = DummyEventDispatcherBuilder::from_config(DUMMY_CONFIG_V4)
            .build::<DummyEventDispatcher>();
        let fragment_id = 0;

        //
        // Test that fragment bodies must be a multiple of
        // `FRAGMENT_BLOCK_SIZE`, except for the last fragment.
        //

        // Process fragment #0
        process_ipv4_fragment(&mut ctx, fragment_id, 0, 2, ExpectedResult::NeedMore);

        // Process fragment #1 (body size is not a multiple of
        // `FRAGMENT_BLOCK_SIZE` and more flag is `true`).
        let mut builder = get_ipv4_builder();
        builder.id(fragment_id);
        builder.fragment_offset(1);
        builder.mf_flag(true);
        // Body with 1 byte less than `FRAGMENT_BLOCK_SIZE` so it is not a
        // multiple of `FRAGMENT_BLOCK_SIZE`.
        let mut body: Vec<u8> = Vec::new();
        body.extend(FRAGMENT_BLOCK_SIZE..FRAGMENT_BLOCK_SIZE * 2 - 1);
        let mut buffer = Buf::new(body, ..).encapsulate(builder).serialize_vec_outer().unwrap();
        let packet = buffer.parse::<Ipv4Packet<_>>().unwrap();
        assert_frag_proc_state_invalid!(process_fragment::<Ipv4, _, &[u8]>(&mut ctx, packet));

        // Process fragment #1 (body size is not a multiple of
        // `FRAGMENT_BLOCK_SIZE` but more flag is `false`). The last fragment is
        // allowed to not be a multiple of `FRAGMENT_BLOCK_SIZE`.
        let mut builder = get_ipv4_builder();
        builder.id(fragment_id);
        builder.fragment_offset(1);
        builder.mf_flag(false);
        // Body with 1 byte less than `FRAGMENT_BLOCK_SIZE` so it is not a
        // multiple of `FRAGMENT_BLOCK_SIZE`.
        let mut body: Vec<u8> = Vec::new();
        body.extend(FRAGMENT_BLOCK_SIZE..FRAGMENT_BLOCK_SIZE * 2 - 1);
        let mut buffer = Buf::new(body, ..).encapsulate(builder).serialize_vec_outer().unwrap();
        let packet = buffer.parse::<Ipv4Packet<_>>().unwrap();
        let (key, packet_len) = assert_frag_proc_state_ready!(
            process_fragment::<Ipv4, _, &[u8]>(&mut ctx, packet),
            DUMMY_CONFIG_V4.remote_ip.get(),
            DUMMY_CONFIG_V4.local_ip.get(),
            fragment_id,
            35
        );
        let mut buffer: Vec<u8> = vec![0; packet_len];
        let mut buffer = &mut buffer[..];
        let packet =
            reassemble_packet::<Ipv4, _, &mut [u8], _>(&mut ctx, &key, &mut buffer).unwrap();
        let mut expected_body: Vec<u8> = Vec::new();
        expected_body.extend(0..15);
        assert_eq!(packet.body(), &expected_body[..]);
    }

    #[test]
    fn test_ipv6_fragment_not_multiple_of_offset_unit() {
        let mut ctx = DummyEventDispatcherBuilder::from_config(DUMMY_CONFIG_V6)
            .build::<DummyEventDispatcher>();
        let fragment_id = 0;

        //
        // Test that fragment bodies must be a multiple of
        // `FRAGMENT_BLOCK_SIZE`, except for the last fragment.
        //

        // Process fragment #0
        process_ipv6_fragment(&mut ctx, fragment_id, 0, 2, ExpectedResult::NeedMore);

        // Process fragment #1 (body size is not a multiple of
        // `FRAGMENT_BLOCK_SIZE` and more flag is `true`).
        let mut bytes = vec![0; 48];
        bytes[..4].copy_from_slice(&[0x60, 0x20, 0x00, 0x77][..]);
        bytes[6] = Ipv6ExtHdrType::Fragment.into(); // Next Header
        bytes[7] = 64;
        bytes[8..24].copy_from_slice(DUMMY_CONFIG_V6.remote_ip.bytes());
        bytes[24..40].copy_from_slice(DUMMY_CONFIG_V6.local_ip.bytes());
        bytes[40] = IpProto::Tcp.into();
        bytes[42] = 0;
        bytes[43] = (1 << 3) | 1;
        NetworkEndian::write_u32(&mut bytes[44..48], fragment_id as u32);
        bytes.extend(FRAGMENT_BLOCK_SIZE..FRAGMENT_BLOCK_SIZE * 2 - 1);
        let payload_len = (bytes.len() - 40) as u16;
        NetworkEndian::write_u16(&mut bytes[4..6], payload_len);
        let mut buf = Buf::new(bytes, ..);
        let packet = buf.parse::<Ipv6Packet<_>>().unwrap();
        assert_frag_proc_state_invalid!(process_fragment::<Ipv6, _, &[u8]>(&mut ctx, packet));

        // Process fragment #1 (body size is not a multiple of
        // `FRAGMENT_BLOCK_SIZE` but more flag is `false`). The last fragment is
        // allowed to not be a multiple of `FRAGMENT_BLOCK_SIZE`.
        let mut bytes = vec![0; 48];
        bytes[..4].copy_from_slice(&[0x60, 0x20, 0x00, 0x77][..]);
        bytes[6] = Ipv6ExtHdrType::Fragment.into(); // Next Header
        bytes[7] = 64;
        bytes[8..24].copy_from_slice(DUMMY_CONFIG_V6.remote_ip.bytes());
        bytes[24..40].copy_from_slice(DUMMY_CONFIG_V6.local_ip.bytes());
        bytes[40] = IpProto::Tcp.into();
        bytes[42] = 0;
        bytes[43] = (1 << 3);
        NetworkEndian::write_u32(&mut bytes[44..48], fragment_id as u32);
        bytes.extend(FRAGMENT_BLOCK_SIZE..FRAGMENT_BLOCK_SIZE * 2 - 1);
        let payload_len = (bytes.len() - 40) as u16;
        NetworkEndian::write_u16(&mut bytes[4..6], payload_len);
        let mut buf = Buf::new(bytes, ..);
        let packet = buf.parse::<Ipv6Packet<_>>().unwrap();
        let (key, packet_len) = assert_frag_proc_state_ready!(
            process_fragment::<Ipv6, _, &[u8]>(&mut ctx, packet),
            DUMMY_CONFIG_V6.remote_ip.get(),
            DUMMY_CONFIG_V6.local_ip.get(),
            fragment_id,
            55
        );
        let mut buffer: Vec<u8> = vec![0; packet_len];
        let mut buffer = &mut buffer[..];
        let packet =
            reassemble_packet::<Ipv6, _, &mut [u8], _>(&mut ctx, &key, &mut buffer).unwrap();
        let mut expected_body: Vec<u8> = Vec::new();
        expected_body.extend(0..15);
        assert_eq!(packet.body(), &expected_body[..]);
    }

    #[ip_test]
    fn test_ip_reassembly_with_multiple_intertwined_packets<I: Ip>() {
        let mut ctx = DummyEventDispatcherBuilder::from_config(get_dummy_config::<I::Addr>())
            .build::<DummyEventDispatcher>();
        let fragment_id_0 = 5;
        let fragment_id_1 = 10;

        //
        // Test that we properly reassemble fragmented packets when they arrive
        // intertwined with other packets' fragments.
        //

        // Process fragment #0 for packet #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_0, 0, 3, ExpectedResult::NeedMore);

        // Process fragment #0 for packet #1
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_1, 0, 3, ExpectedResult::NeedMore);

        // Process fragment #1 for packet #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_0, 1, 3, ExpectedResult::NeedMore);

        // Process fragment #1 for packet #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_1, 1, 3, ExpectedResult::NeedMore);

        // Process fragment #2 for packet #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_0, 2, 3, ExpectedResult::ReadyReassemble);

        // Process fragment #2 for packet #1
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_1, 2, 3, ExpectedResult::ReadyReassemble);
    }

    #[ip_test]
    fn test_ip_reassembly_timer_with_multiple_intertwined_packets<I: Ip>() {
        let mut ctx = DummyEventDispatcherBuilder::from_config(get_dummy_config::<I::Addr>())
            .build::<DummyEventDispatcher>();
        let fragment_id_0 = 5;
        let fragment_id_1 = 10;
        let fragment_id_2 = 15;

        //
        // Test that we properly timer with multiple intertwined packets that
        // all arrive out of order. We expect packet 1 and 3 to succeed, and
        // packet 1 to fail due to the reassembly timer.
        //
        // The flow of events:
        //   T=0s:
        //     - Packet #0, Fragment #0 arrives (timer scheduled for T=60s).
        //     - Packet #1, Fragment #2 arrives (timer scheduled for T=60s).
        //     - Packet #2, Fragment #2 arrives (timer scheduled for T=60s).
        //   T=30s:
        //     - Packet #0, Fragment #2 arrives.
        //   T=40s:
        //     - Packet #2, Fragment #1 arrives.
        //     - Packet #0, Fragment #1 arrives (timer cancelled since all
        //       fragments arrived).
        //   T=50s:
        //     - Packet #1, Fragment #0 arrives.
        //     - Packet #2, Fragment #0 arrives (timer cancelled since all
        //       fragments arrived).
        //   T=60s:
        //     - Timeout for reassembly of Packet #1.
        //     - Packet #1, Fragment #1 arrives (final fragment but timer
        //       already triggered so fragment not complete).
        //

        // Process fragment #0 for packet #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_0, 0, 3, ExpectedResult::NeedMore);

        // Process fragment #1 for packet #1
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_1, 2, 3, ExpectedResult::NeedMore);

        // Process fragment #2 for packet #2
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_2, 2, 3, ExpectedResult::NeedMore);

        // Advance time by 30s (should be at 30s now).
        assert_eq!(run_for(&mut ctx, Duration::from_secs(30)), 0);

        // Process fragment #2 for packet #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_0, 2, 3, ExpectedResult::NeedMore);

        // Advance time by 10s (should be at 40s now).
        assert_eq!(run_for(&mut ctx, Duration::from_secs(10)), 0);

        // Process fragment #1 for packet #2
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_2, 1, 3, ExpectedResult::NeedMore);

        // Process fragment #1 for packet #0
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_0, 1, 3, ExpectedResult::ReadyReassemble);

        // Advance time by 10s (should be at 50s now).
        assert_eq!(run_for(&mut ctx, Duration::from_secs(10)), 0);

        // Process fragment #0 for packet #1
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_1, 0, 3, ExpectedResult::NeedMore);

        // Process fragment #0 for packet #2
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_2, 0, 3, ExpectedResult::ReadyReassemble);

        // Advance time by 10s (should be at 60s now)), triggering the timer for
        // the reassembly of packet #1
        assert_eq!(run_for(&mut ctx, Duration::from_secs(10)), 1);

        // Make sure no other times exist.
        assert_eq!(ctx.dispatcher.timer_events().count(), 0);

        // Process fragment #2 for packet #1 Should get a need more return value
        // since even though we technically received all the fragments, the last
        // fragment didn't arrive until after the reassembly timer.
        process_ip_fragment::<I, _>(&mut ctx, fragment_id_1, 2, 3, ExpectedResult::NeedMore);
    }
}
