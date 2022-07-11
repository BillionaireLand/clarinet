use crate::indexer::{ChainSegment, ChainSegmentIncompatibility};
use crate::utils::AbstractBlock;
use bitcoincore_rpc::bitcoin::Block;
use clarity_repl::clarity::util::hash::to_hex;
use orchestra_types::{
    BitcoinChainEvent, BlockIdentifier, Chain, StacksBlockData, StacksBlockUpdate,
    StacksChainEvent, StacksChainUpdatedWithBlocksData, StacksChainUpdatedWithMicroblocksData,
    StacksChainUpdatedWithMicroblocksReorgData, StacksChainUpdatedWithReorgData,
    StacksMicroblockData, StacksMicroblocksTrail, StacksTransactionData,
};
use std::collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

pub struct StacksBlockPool {
    canonical_fork_id: usize,
    orphans: BTreeSet<BlockIdentifier>,
    block_store: HashMap<BlockIdentifier, StacksBlockData>,
    forks: BTreeMap<usize, ChainSegment>,
    microblock_store: HashMap<(BlockIdentifier, BlockIdentifier), StacksMicroblockData>,
    micro_forks: HashMap<BlockIdentifier, Vec<ChainSegment>>,
    micro_orphans: BTreeSet<(BlockIdentifier, BlockIdentifier)>,
    canonical_micro_fork_id: HashMap<BlockIdentifier, usize>,
}

impl StacksBlockPool {
    pub fn new() -> StacksBlockPool {
        let mut forks = BTreeMap::new();
        forks.insert(0, ChainSegment::new());
        StacksBlockPool {
            canonical_fork_id: 0,
            block_store: HashMap::new(),
            orphans: BTreeSet::new(),
            forks,
            microblock_store: HashMap::new(),
            micro_forks: HashMap::new(),
            micro_orphans: BTreeSet::new(),
            canonical_micro_fork_id: HashMap::new(),
        }
    }

    pub fn process_block(
        &mut self,
        block: StacksBlockData,
    ) -> Result<Option<StacksChainEvent>, ()> {
        info!("Start processing block {}", block.block_identifier);

        // Keep block data in memory
        let existing_entry = self
            .block_store
            .insert(block.block_identifier.clone(), block.clone());
        if existing_entry.is_some() {
            warn!(
                "Block {} has already been processed",
                block.block_identifier
            );
            return Ok(None);
        }

        for (i, fork) in self.forks.iter() {
            info!("Active fork {}: {}", i, fork);
        }
        // Retrieve previous canonical fork
        let previous_canonical_fork_id = self.canonical_fork_id;
        let previous_canonical_fork = match self.forks.get(&previous_canonical_fork_id) {
            Some(fork) => fork.clone(),
            None => return Err(()),
        };

        let mut fork_updated = None;
        for (_, fork) in self.forks.iter_mut() {
            let (block_appended, mut new_fork) = fork.try_append_block(&block);
            if block_appended {
                if let Some(new_fork) = new_fork.take() {
                    let fork_id = self.forks.len();
                    self.forks.insert(fork_id, new_fork);
                    fork_updated = self.forks.get_mut(&fork_id);
                } else {
                    fork_updated = Some(fork);
                }
                // A block can only be added to one segment
                break;
            }
        }

        let fork_updated = match fork_updated.take() {
            Some(fork) => {
                info!(
                    "Block {} successfully appended to {}",
                    block.block_identifier, fork
                );
                fork
            }
            None => {
                info!(
                    "Unable to process block {} - inboxed for later",
                    block.block_identifier
                );
                self.orphans.insert(block.block_identifier.clone());
                return Ok(None);
            }
        };

        // Process former orphans
        let orphans = self.orphans.clone();
        let mut orphans_to_untrack = HashSet::new();

        let mut at_least_one_orphan_appended = true;
        // As long as we are successful appending blocks that were previously unprocessable,
        // Keep looping on this backlog
        let mut applied = HashSet::new();
        while at_least_one_orphan_appended {
            at_least_one_orphan_appended = false;
            for orphan_block_identifier in orphans.iter() {
                if applied.contains(orphan_block_identifier) {
                    continue;
                }
                let block = match self.block_store.get(orphan_block_identifier) {
                    Some(block) => block.clone(),
                    None => continue,
                };

                let (orphan_appended, new_fork) = fork_updated.try_append_block(&block);
                if orphan_appended {
                    applied.insert(orphan_block_identifier);
                    orphans_to_untrack.insert(orphan_block_identifier);
                }
                at_least_one_orphan_appended = at_least_one_orphan_appended || orphan_appended;
            }
        }

        // Update orphans
        for orphan in orphans_to_untrack.into_iter() {
            info!("Dequeuing orphan {}", orphan);
            self.orphans.remove(orphan);
        }

        // Select canonical fork
        let mut canonical_fork_id = 0;
        let mut highest_height = 0;
        let mut highest_btc_spent = 0;
        for (fork_id, fork) in self.forks.iter() {
            info!("Active fork: {} - {}", fork_id, fork);
            if fork.get_length() >= highest_height {
                highest_height = fork.get_length();
                if fork.amount_of_btc_spent > highest_btc_spent
                    || (fork.amount_of_btc_spent == highest_btc_spent
                        && fork_id > &canonical_fork_id)
                {
                    highest_btc_spent = fork.amount_of_btc_spent;
                    canonical_fork_id = *fork_id;
                }
            }
        }
        info!("Active fork selected as canonical: {}", canonical_fork_id);

        self.canonical_fork_id = canonical_fork_id;
        // Generate chain event from the previous and current canonical forks
        let canonical_fork = self.forks.get(&canonical_fork_id).unwrap().clone();
        if canonical_fork.eq(&previous_canonical_fork) {
            info!("Canonical fork unchanged");
            return Ok(None);
        }

        let res = self.generate_block_chain_event(&canonical_fork, &previous_canonical_fork);
        let mut chain_event = match res {
            Ok(chain_event) => chain_event,
            Err(ChainSegmentIncompatibility::ParentBlockUnknown) => {
                self.canonical_fork_id = previous_canonical_fork_id;
                return Ok(None);
            }
            _ => return Ok(None),
        };

        self.collect_and_prune_confirmed_blocks(&mut chain_event);

        Ok(Some(chain_event))
    }

    pub fn collect_and_prune_confirmed_blocks(&mut self, chain_event: &mut StacksChainEvent) {
        let (tip, confirmed_blocks) = match chain_event {
            StacksChainEvent::ChainUpdatedWithBlocks(ref mut event) => {
                match event.new_blocks.last() {
                    Some(tip) => (
                        tip.block.block_identifier.clone(),
                        &mut event.confirmed_blocks,
                    ),
                    None => return,
                }
            }
            StacksChainEvent::ChainUpdatedWithReorg(ref mut event) => {
                match event.blocks_to_apply.last() {
                    Some(tip) => (
                        tip.block.block_identifier.clone(),
                        &mut event.confirmed_blocks,
                    ),
                    None => return,
                }
            }
            _ => return,
        };

        let mut forks_to_prune = vec![];
        let mut ancestor_identifier = &tip;

        // Retrieve the whole canonical segment present in memory, ascending order
        // [1] ... [6] [7]
        let canonical_segment = {
            let mut segment = vec![];
            while let Some(ancestor) = self.block_store.get(&ancestor_identifier) {
                ancestor_identifier = &ancestor.parent_block_identifier;
                segment.push(ancestor.block_identifier.clone());
            }
            segment
        };
        if canonical_segment.len() < 7 {
            return;
        }
        // Any block beyond 6th ancestor is considered as confirmed and can be pruned
        let cut_off = &canonical_segment[5];

        // Prune forks using the confirmed block
        let mut blocks_to_prune = vec![];
        for (fork_id, fork) in self.forks.iter_mut() {
            let mut res = fork.prune_confirmed_blocks(&cut_off);
            blocks_to_prune.append(&mut res);
            if fork.block_ids.is_empty() {
                forks_to_prune.push(*fork_id);
            }
        }

        // Prune orphans using the confirmed block
        let iter = self.orphans.clone().into_iter();
        for orphan in iter {
            if orphan.index < cut_off.index {
                self.orphans.remove(&orphan);
                blocks_to_prune.push(orphan);
            }
        }

        let mut blocks = vec![];
        let mut trails = vec![];
        // Looping a first time, to collect:
        // 1) the blocks that we will be returning
        // 2) the tip of the trail confirmed by the subsequent block
        // Block 6 (index 5) is confirming transactions included in microblocks
        // that must be merged in Block 7.
        for (i, confirmed_block) in canonical_segment[5..].into_iter().enumerate() {
            if i == 0 {
                let block = match self.block_store.get(confirmed_block) {
                    None => {
                        error!("unable to retrieve data for {}", confirmed_block);
                        return;
                    }
                    Some(block) => block,
                };
                trails.push(block.metadata.confirm_microblock_identifier.clone());
            } else {
                let block = match self.block_store.remove(confirmed_block) {
                    None => {
                        error!("unable to retrieve data for {}", confirmed_block);
                        return;
                    }
                    Some(block) => block,
                };
                trails.push(block.metadata.confirm_microblock_identifier.clone());
                blocks.push(block);
            }
        }

        for (mut block, trail) in blocks.into_iter().zip(trails) {
            if let Some(trail_tip) = trail {
                // The subsequent block was confirming a trail of microblock
                let canonical_micro_fork_id =
                    match self.canonical_micro_fork_id.remove(&block.block_identifier) {
                        None => {
                            error!(
                                "unable to retrieve canonical_micro_fork_id for {}",
                                block.block_identifier
                            );
                            return;
                        }
                        Some(id) => id,
                    };
                let mut segment = match self.micro_forks.remove(&block.block_identifier) {
                    None => {
                        error!(
                            "unable to retrieve canonical_micro_fork for {}",
                            block.block_identifier
                        );
                        return;
                    }
                    Some(mut microforks) => microforks.remove(canonical_micro_fork_id),
                };
                // Sanity check
                let tip = match segment.block_ids.pop_front() {
                    None => {
                        error!("canonical micro fork empty {}", block.block_identifier);
                        return;
                    }
                    Some(id) => id,
                };
                if !tip.eq(&trail_tip) {
                    error!(
                        "canonical micro fork mismatch for {}",
                        block.block_identifier
                    );
                    return;
                }
                // Replace the tip
                segment.block_ids.push_front(tip);
                while let Some(entry) = segment.block_ids.pop_back() {
                    let mut microblock = match self
                        .microblock_store
                        .remove(&(block.block_identifier.clone(), entry.clone()))
                    {
                        None => {
                            error!("unable to retrieve microblock data for {}", entry);
                            return;
                        }
                        Some(microblock) => microblock,
                    };
                    block.transactions.append(&mut microblock.transactions);
                }
            }
            confirmed_blocks.push(block);
        }

        // Prune data
        for block_to_prune in blocks_to_prune {
            self.block_store.remove(&block_to_prune);
            self.micro_forks.remove(&block_to_prune);
            self.canonical_micro_fork_id.remove(&block_to_prune);
            // TODO(lgalabru): cascade pruning down to micro_orphans and microblocks_store
        }
        for fork_id in forks_to_prune {
            self.forks.remove(&fork_id);
        }
        confirmed_blocks.reverse();
    }

    pub fn process_microblocks(
        &mut self,
        microblocks: Vec<StacksMicroblockData>,
    ) -> Option<StacksChainEvent> {
        // Retrieve anchor block being updated
        let anchor_block_updated = match microblocks.first() {
            Some(microcblock) => {
                info!(
                    "Start processing microblocks anchored to {}",
                    microcblock.metadata.anchor_block_identifier
                );
                microcblock.metadata.anchor_block_identifier.clone()
            }
            None => {
                error!("No microblock to process");
                return None;
            }
        };

        let mut previous_canonical_micro_fork = None;

        if let (Some(microforks), Some(micro_fork_id)) = (
            self.micro_forks.get(&anchor_block_updated),
            self.canonical_micro_fork_id.get(&anchor_block_updated),
        ) {
            info!(
                "Previous fork selected as canonical: {}",
                microforks[*micro_fork_id]
            );
            previous_canonical_micro_fork = Some(microforks[*micro_fork_id].clone());
        }

        let mut micro_forks_updated = HashSet::new();

        for mut microblock in microblocks.into_iter() {
            // Temporary patch: the event observer is not returning the block height of the anchor block,
            // we're using the local state to retrieve this missing piece of data.
            if let Some(block) = self
                .block_store
                .get(&microblock.metadata.anchor_block_identifier)
            {
                microblock.metadata.anchor_block_identifier = block.block_identifier.clone();
            }
            // Keep microblock data in memory
            self.microblock_store.insert(
                (
                    microblock.metadata.anchor_block_identifier.clone(),
                    microblock.block_identifier.clone(),
                ),
                microblock.clone(),
            );

            let mut micro_fork_updated = None;

            if microblock.block_identifier.index == 0 {
                info!("Initiating new micro fork {}", microblock.block_identifier);
                let mut microfork = ChainSegment::new();
                microfork.append_block_identifier(&&microblock.block_identifier);

                match self.micro_forks.entry(anchor_block_updated.clone()) {
                    Entry::Occupied(microforks) => microforks.into_mut().push(microfork),
                    Entry::Vacant(v) => {
                        v.insert(vec![microfork]);
                    }
                };
                micro_fork_updated = self
                    .micro_forks
                    .get_mut(&anchor_block_updated)
                    .and_then(|microfork| microfork.last_mut())
            } else {
                if let Some(microforks) = self.micro_forks.get_mut(&anchor_block_updated) {
                    for micro_fork in microforks.iter_mut() {
                        let (block_appended, mut new_micro_fork) =
                            micro_fork.try_append_block(&microblock);
                        if block_appended {
                            if let Some(new_micro_fork) = new_micro_fork.take() {
                                microforks.push(new_micro_fork);
                                micro_fork_updated = microforks.last_mut();
                            } else {
                                micro_fork_updated = Some(micro_fork);
                            }
                            // A block can only be added to one segment
                            break;
                        }
                    }
                }
            }

            let micro_fork_updated = match micro_fork_updated.take() {
                Some(micro_fork) => micro_fork,
                None => {
                    self.micro_orphans.insert((
                        microblock.metadata.anchor_block_identifier.clone(),
                        microblock.block_identifier.clone(),
                    ));
                    continue;
                }
            };

            // Process former orphans
            let orphans = self.micro_orphans.clone();
            let mut orphans_to_untrack = HashSet::new();

            let mut at_least_one_orphan_appended = true;
            // As long as we are successful appending blocks that were previously unprocessable,
            // Keep looping on this backlog
            let mut applied = HashSet::new();
            while at_least_one_orphan_appended {
                at_least_one_orphan_appended = false;
                for orphan_key in orphans.iter() {
                    if applied.contains(orphan_key) {
                        continue;
                    }
                    let block = match self.microblock_store.get(orphan_key) {
                        Some(block) => block.clone(),
                        None => continue,
                    };

                    let (orphan_appended, new_fork) = micro_fork_updated.try_append_block(&block);
                    if orphan_appended {
                        applied.insert(orphan_key);
                        orphans_to_untrack.insert(orphan_key);
                    }
                    at_least_one_orphan_appended = at_least_one_orphan_appended || orphan_appended;
                }
            }

            // Update orphans
            for orphan in orphans_to_untrack.into_iter() {
                info!("Dequeuing orphaned microblock ({}, {})", orphan.0, orphan.1);
                self.micro_orphans.remove(orphan);
            }

            micro_forks_updated.insert(microblock.metadata.anchor_block_identifier);
        }

        if micro_forks_updated.is_empty() {
            info!("Unable to process microblocks - inboxed for later");
            return None;
        } else {
            info!("Microblocks successfully appended");
        }

        assert_eq!(micro_forks_updated.len(), 1);
        let microforks = {
            let microforks = self
                .micro_forks
                .get(&anchor_block_updated)
                .expect("unable to retrieve microforks");
            microforks
        };

        // Select canonical fork
        let mut canonical_micro_fork_id = 0;
        let mut highest_height = 0;
        for (fork_id, fork) in microforks.iter().enumerate() {
            info!("Active microfork: {} - {}", fork_id, fork);
            if fork.get_length() >= highest_height {
                highest_height = fork.get_length();
                canonical_micro_fork_id = fork_id;
            }
        }

        self.canonical_micro_fork_id
            .insert(anchor_block_updated.clone(), canonical_micro_fork_id);

        // Generate chain event from the previous and current canonical forks
        let canonical_micro_fork = microforks.get(canonical_micro_fork_id).unwrap();

        info!(
            "Active microfork selected as canonical: {}",
            canonical_micro_fork
        );

        let chain_event = self.generate_microblock_chain_event(
            &anchor_block_updated,
            canonical_micro_fork,
            &previous_canonical_micro_fork,
        );

        chain_event
    }

    // We got the confirmed canonical microblock trail,
    // and we want to send a diff with whatever was sent
    // in the past.
    // The issue comes from the following case. If we
    // condider the 3 following forks
    //
    // 1) A1 - B1 - a1 - b1 - c1 - C1
    //
    // 2) A1 - B1 - a1 - b1 - C2
    //
    // 3) A1 - B1 - a1 - b1 - c1 - d1 - C3
    //
    // How can we always be sending back the right diff?
    // As is, if 1) 2) 3), we will be sending:
    // - BlockUpdate(C1)
    // - BlockReorg(C2, rollback: [c1], apply: [])
    // - BlockReorg(C3, rollback: [], apply: [c1, d1])

    pub fn confirm_microblocks_for_block(
        &mut self,
        block: &StacksBlockData,
        diff_enabled: bool,
    ) -> (Option<StacksChainEvent>, Option<Vec<StacksMicroblockData>>) {
        match (
            &block.metadata.confirm_microblock_identifier,
            self.micro_forks.get(&block.parent_block_identifier),
        ) {
            (Some(last_microblock), Some(microforks)) => {
                let previous_canonical_segment = match self
                    .canonical_micro_fork_id
                    .get(&block.parent_block_identifier)
                {
                    Some(id) => Some(microforks[*id].clone()),
                    None => None,
                };

                let mut new_canonical_segment = None;
                for (microfork_id, microfork) in microforks.iter().enumerate() {
                    self.canonical_micro_fork_id
                        .insert(block.parent_block_identifier.clone(), microfork_id);
                    if microfork.block_ids.contains(&last_microblock) {
                        let mut confirmed_microblocks = microfork.clone();
                        let (_, mutated) = confirmed_microblocks
                            .keep_blocks_from_oldest_to_block_identifier(&last_microblock);
                        new_canonical_segment = Some((
                            confirmed_microblocks,
                            if mutated {
                                microforks.len()
                            } else {
                                microfork_id
                            },
                        ));
                        break;
                    }
                }

                if let Some((new_canonical_segment, microfork_id)) = new_canonical_segment {
                    let result = if diff_enabled {
                        let chain_event = self.generate_microblock_chain_event(
                            &block.parent_block_identifier,
                            &new_canonical_segment,
                            &previous_canonical_segment,
                        );
                        (chain_event, None)
                    } else {
                        (
                            None,
                            Some(self.get_microblocks_data(
                                &block.parent_block_identifier,
                                &new_canonical_segment,
                            )),
                        )
                    };
                    // insert confirmed_microblocks in self.micro_forks
                    self.canonical_micro_fork_id
                        .insert(block.parent_block_identifier.clone(), microfork_id);

                    // update self.canonical_micro_fork_id
                    match self
                        .micro_forks
                        .entry(block.parent_block_identifier.clone())
                    {
                        Entry::Occupied(microforks) => {
                            microforks.into_mut().push(new_canonical_segment)
                        }
                        Entry::Vacant(v) => {
                            v.insert(vec![new_canonical_segment]);
                        }
                    };
                    return result;
                }
                (None, None)
            }
            _ => (None, None),
        }
    }

    pub fn get_microblocks_data(
        &self,
        anchor_block_identifier: &BlockIdentifier,
        chain_segment: &ChainSegment,
    ) -> Vec<StacksMicroblockData> {
        let mut microblocks = vec![];
        for i in 0..chain_segment.block_ids.len() {
            let block_identifier = &chain_segment.block_ids[chain_segment.block_ids.len() - 1 - i];
            let microblock_identifier = (anchor_block_identifier.clone(), block_identifier.clone());
            let block = match self.microblock_store.get(&microblock_identifier) {
                Some(block) => block.clone(),
                None => panic!("unable to retrive microblock from microblock store"),
            };
            microblocks.push(block)
        }
        microblocks
    }

    pub fn get_confirmed_parent_microblocks(
        &self,
        block: &StacksBlockData,
    ) -> Vec<StacksMicroblockData> {
        match self.micro_forks.get(&block.parent_block_identifier) {
            Some(microforks) => {
                let previous_canonical_segment = match self
                    .canonical_micro_fork_id
                    .get(&block.parent_block_identifier)
                {
                    Some(id) => {
                        self.get_microblocks_data(&block.parent_block_identifier, &microforks[*id])
                    }
                    None => vec![],
                };
                previous_canonical_segment
            }
            _ => vec![],
        }
    }

    pub fn generate_block_chain_event(
        &mut self,
        canonical_segment: &ChainSegment,
        other_segment: &ChainSegment,
    ) -> Result<StacksChainEvent, ChainSegmentIncompatibility> {
        if other_segment.is_empty() {
            let mut new_blocks = vec![];
            for i in 0..canonical_segment.block_ids.len() {
                let block_identifier =
                    &canonical_segment.block_ids[canonical_segment.block_ids.len() - 1 - i];
                let block = match self.block_store.get(block_identifier) {
                    Some(block) => block.clone(),
                    None => {
                        error!(
                            "unable to retrive block {} from block store",
                            block_identifier
                        );
                        return Err(ChainSegmentIncompatibility::Unknown);
                    }
                };
                let block_update = match self.confirm_microblocks_for_block(&block, true) {
                    (Some(ref mut chain_event), _) => {
                        let mut update = StacksBlockUpdate::new(block);
                        match chain_event {
                            StacksChainEvent::ChainUpdatedWithMicroblocks(data) => {
                                update
                                    .parent_microblocks_to_apply
                                    .append(&mut data.new_microblocks);
                            }
                            StacksChainEvent::ChainUpdatedWithMicroblocksReorg(data) => {
                                update
                                    .parent_microblocks_to_apply
                                    .append(&mut data.microblocks_to_apply);
                                update
                                    .parent_microblocks_to_rollback
                                    .append(&mut data.microblocks_to_rollback);
                            }
                            _ => unreachable!(),
                        };
                        update
                    }
                    _ => StacksBlockUpdate::new(block),
                };
                new_blocks.push(block_update)
            }
            return Ok(StacksChainEvent::ChainUpdatedWithBlocks(
                StacksChainUpdatedWithBlocksData {
                    new_blocks,
                    confirmed_blocks: vec![],
                },
            ));
        }
        if let Ok(divergence) = canonical_segment.try_identify_divergence(other_segment, false) {
            if divergence.blocks_to_rollback.is_empty() {
                let mut new_blocks = vec![];
                for i in 0..divergence.blocks_to_apply.len() {
                    let block_identifier = &divergence.blocks_to_apply[i];
                    let block = match self.block_store.get(block_identifier) {
                        Some(block) => block.clone(),
                        None => panic!("unable to retrive block from block store"),
                    };
                    let block_update = match self.confirm_microblocks_for_block(&block, true) {
                        (Some(ref mut chain_event), None) => {
                            let mut update = StacksBlockUpdate::new(block);
                            match chain_event {
                                StacksChainEvent::ChainUpdatedWithMicroblocks(data) => {
                                    update
                                        .parent_microblocks_to_apply
                                        .append(&mut data.new_microblocks);
                                }
                                StacksChainEvent::ChainUpdatedWithMicroblocksReorg(data) => {
                                    update
                                        .parent_microblocks_to_apply
                                        .append(&mut data.microblocks_to_apply);
                                    update
                                        .parent_microblocks_to_rollback
                                        .append(&mut data.microblocks_to_rollback);
                                }
                                _ => unreachable!(),
                            };
                            update
                        }
                        _ => StacksBlockUpdate::new(block),
                    };
                    new_blocks.push(block_update)
                }
                return Ok(StacksChainEvent::ChainUpdatedWithBlocks(
                    StacksChainUpdatedWithBlocksData {
                        new_blocks,
                        confirmed_blocks: vec![],
                    },
                ));
            } else {
                return Ok(StacksChainEvent::ChainUpdatedWithReorg(
                    StacksChainUpdatedWithReorgData {
                        blocks_to_rollback: divergence
                            .blocks_to_rollback
                            .iter()
                            .map(|block_id| {
                                let block = match self.block_store.get(block_id) {
                                    Some(block) => block.clone(),
                                    None => panic!("unable to retrive block from block store"),
                                };
                                let parent_microblocks_to_rollback =
                                    self.get_confirmed_parent_microblocks(&block);
                                let mut update = StacksBlockUpdate::new(block);
                                update.parent_microblocks_to_rollback =
                                    parent_microblocks_to_rollback;
                                update
                            })
                            .collect::<Vec<_>>(),
                        blocks_to_apply: divergence
                            .blocks_to_apply
                            .iter()
                            .map(|block_id| {
                                let block = match self.block_store.get(block_id) {
                                    Some(block) => block.clone(),
                                    None => panic!("unable to retrive block from block store"),
                                };
                                let block_update = match self
                                    .confirm_microblocks_for_block(&block, false)
                                {
                                    (_, Some(microblocks_to_apply)) => {
                                        let mut update = StacksBlockUpdate::new(block);
                                        update.parent_microblocks_to_apply = microblocks_to_apply;
                                        update
                                    }
                                    _ => StacksBlockUpdate::new(block),
                                };
                                block_update
                            })
                            .collect::<Vec<_>>(),
                        confirmed_blocks: vec![],
                    },
                ));
            }
        }
        info!(
            "Unable to infer chain event out of {} and {}",
            canonical_segment, other_segment
        );
        Err(ChainSegmentIncompatibility::ParentBlockUnknown)
    }

    pub fn generate_microblock_chain_event(
        &self,
        anchor_block_identifier: &BlockIdentifier,
        new_canonical_segment: &ChainSegment,
        previous_canonical_segment: &Option<ChainSegment>,
    ) -> Option<StacksChainEvent> {
        let previous_canonical_segment = match previous_canonical_segment {
            Some(previous_canonical_segment) if !previous_canonical_segment.is_empty() => {
                previous_canonical_segment
            }
            _ => {
                let mut new_microblocks = vec![];
                for i in 0..new_canonical_segment.block_ids.len() {
                    let block_identifier = &new_canonical_segment.block_ids
                        [new_canonical_segment.block_ids.len() - 1 - i];
                    let microblock_identifier =
                        (anchor_block_identifier.clone(), block_identifier.clone());
                    let block = match self.microblock_store.get(&microblock_identifier) {
                        Some(block) => block.clone(),
                        None => panic!("unable to retrive microblock from microblock store"),
                    };
                    new_microblocks.push(block)
                }
                return Some(StacksChainEvent::ChainUpdatedWithMicroblocks(
                    StacksChainUpdatedWithMicroblocksData { new_microblocks },
                ));
            }
        };

        if new_canonical_segment.eq(&previous_canonical_segment) {
            return None;
        }

        if let Ok(divergence) =
            new_canonical_segment.try_identify_divergence(previous_canonical_segment, true)
        {
            if divergence.blocks_to_rollback.is_empty() {
                let mut new_microblocks = vec![];
                for i in 0..divergence.blocks_to_apply.len() {
                    let block_identifier = &new_canonical_segment.block_ids[i];

                    let microblock_identifier =
                        (anchor_block_identifier.clone(), block_identifier.clone());
                    let block = match self.microblock_store.get(&microblock_identifier) {
                        Some(block) => block.clone(),
                        None => {
                            panic!("unable to retrive microblock from microblock store")
                        }
                    };
                    new_microblocks.push(block);
                }
                return Some(StacksChainEvent::ChainUpdatedWithMicroblocks(
                    StacksChainUpdatedWithMicroblocksData { new_microblocks },
                ));
            } else {
                return Some(StacksChainEvent::ChainUpdatedWithMicroblocksReorg(
                    StacksChainUpdatedWithMicroblocksReorgData {
                        microblocks_to_rollback: divergence
                            .blocks_to_rollback
                            .iter()
                            .map(|microblock_identifier| {
                                let microblock_identifier = (
                                    anchor_block_identifier.clone(),
                                    microblock_identifier.clone(),
                                );
                                let block = match self.microblock_store.get(&microblock_identifier)
                                {
                                    Some(block) => block.clone(),
                                    None => {
                                        panic!("unable to retrive microblock from microblock store")
                                    }
                                };
                                block
                            })
                            .collect::<Vec<_>>(),
                        microblocks_to_apply: divergence
                            .blocks_to_apply
                            .iter()
                            .map(|microblock_identifier| {
                                let microblock_identifier = (
                                    anchor_block_identifier.clone(),
                                    microblock_identifier.clone(),
                                );
                                let block = match self.microblock_store.get(&microblock_identifier)
                                {
                                    Some(block) => block.clone(),
                                    None => {
                                        panic!("unable to retrive microblock from microblock store")
                                    }
                                };
                                block
                            })
                            .collect::<Vec<_>>(),
                    },
                ));
            }
        }
        None
    }
}