#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::sync::{broadcast, mpsc, oneshot};

    use crate::{
        consensus::consensus::SystemMessage,
        core::util::to_farcaster_time,
        mempool::mempool::{self, Mempool, MempoolMessagesRequest},
        network::gossip::{Config, SnapchainGossip},
        proto::{
            self, FnameTransfer, Height, ShardChunk, ShardHeader, Transaction, UserNameProof,
            UserNameType, ValidatorMessage,
        },
        storage::store::{
            engine::{MempoolMessage, ShardEngine},
            test_helper,
        },
        utils::{
            factory::{events_factory, messages_factory},
            statsd_wrapper::StatsdClientWrapper,
        },
    };

    use self::test_helper::{default_custody_address, default_signer};

    use std::time::Duration;

    use crate::mempool::mempool::{MempoolMessageWithSource, MempoolSource};
    use libp2p::identity::ed25519::Keypair;

    const HOST_FOR_TEST: &str = "127.0.0.1";
    const PORT_FOR_TEST: u32 = 9388;

    fn setup_config(port: u32) -> Config {
        Config::new(
            format!("/ip4/{HOST_FOR_TEST}/udp/{port}/quic-v1"),
            "".to_string(),
        )
    }

    fn setup(
        config: Config,
    ) -> (
        ShardEngine,
        SnapchainGossip,
        Mempool,
        mpsc::Sender<MempoolMessageWithSource>,
        mpsc::Sender<MempoolMessagesRequest>,
        broadcast::Sender<ShardChunk>,
        mpsc::Receiver<SystemMessage>,
    ) {
        let keypair = Keypair::generate();
        let statsd_client = StatsdClientWrapper::new(
            cadence::StatsdClient::builder("", cadence::NopMetricSink {}).build(),
            true,
        );

        let (system_tx, system_rx) = mpsc::channel::<SystemMessage>(100);
        let (mempool_tx, mempool_rx) = mpsc::channel(100);
        let (messages_request_tx, messages_request_rx) = mpsc::channel(100);
        let (shard_decision_tx, shard_decision_rx) = broadcast::channel(100);
        let (engine, _) = test_helper::new_engine();
        let mut shard_senders = HashMap::new();
        shard_senders.insert(1, engine.get_senders());
        let mut shard_stores = HashMap::new();
        shard_stores.insert(1, engine.get_stores());

        let gossip = SnapchainGossip::create(keypair.clone(), &config, system_tx, false).unwrap();

        let mempool = Mempool::new(
            mempool::Config::default(),
            mempool_rx,
            messages_request_rx,
            1,
            shard_stores,
            gossip.tx.clone(),
            shard_decision_rx,
            statsd_client,
        );

        (
            engine,
            gossip,
            mempool,
            mempool_tx,
            messages_request_tx,
            shard_decision_tx,
            system_rx,
        )
    }

    #[tokio::test]
    async fn test_duplicate_user_message_is_invalid() {
        let (mut engine, _, mut mempool, _, _, _, _) = setup(setup_config(9300));
        test_helper::register_user(
            1234,
            default_signer(),
            default_custody_address(),
            &mut engine,
        )
        .await;
        let cast = messages_factory::casts::create_cast_add(1234, "hello", None, None);
        let valid = mempool.message_is_valid(&MempoolMessage::UserMessage(cast.clone()));
        assert!(valid);
        test_helper::commit_message(&mut engine, &cast).await;
        let valid = mempool.message_is_valid(&MempoolMessage::UserMessage(cast.clone()));
        assert!(!valid)
    }

    #[tokio::test]
    async fn test_duplicate_onchain_event_is_invalid() {
        let (mut engine, _, mut mempool, _, _, _, _) = setup(setup_config(9301));
        let onchain_event = events_factory::create_rent_event(1234, Some(10), None, false);
        let valid = mempool.message_is_valid(&MempoolMessage::ValidatorMessage(ValidatorMessage {
            on_chain_event: Some(onchain_event.clone()),
            fname_transfer: None,
        }));
        assert!(valid);
        test_helper::commit_event(&mut engine, &onchain_event).await;
        let valid = mempool.message_is_valid(&MempoolMessage::ValidatorMessage(ValidatorMessage {
            on_chain_event: Some(onchain_event.clone()),
            fname_transfer: None,
        }));
        assert!(!valid)
    }

    #[tokio::test]
    async fn test_duplicate_fname_transfer_is_invalid() {
        let (mut engine, _, mut mempool, _, _, _, _) = setup(setup_config(9302));
        test_helper::register_user(
            1,
            default_signer(),
            hex::decode("8773442740c17c9d0f0b87022c722f9a136206ed").unwrap(),
            &mut engine,
        )
        .await;
        let fname_transfer = FnameTransfer {
            id: 1,
            from_fid: 0,
            proof: Some(UserNameProof {
                timestamp: 1628882891,
                name: "farcaster".as_bytes().to_vec(),
                owner: hex::decode("8773442740c17c9d0f0b87022c722f9a136206ed").unwrap(),
                signature: hex::decode("b7181760f14eda0028e0b647ff15f45235526ced3b4ae07fcce06141b73d32960d3253776e62f761363fb8137087192047763f4af838950a96f3885f3c2289c41b").unwrap(),
                fid: 1,
                r#type: UserNameType::UsernameTypeEnsL1 as i32,
            }),
        };
        let valid = mempool.message_is_valid(&MempoolMessage::ValidatorMessage(ValidatorMessage {
            on_chain_event: None,
            fname_transfer: Some(fname_transfer.clone()),
        }));
        assert!(valid);
        test_helper::commit_fname_transfer(&mut engine, &fname_transfer).await;
        let valid = mempool.message_is_valid(&MempoolMessage::ValidatorMessage(ValidatorMessage {
            on_chain_event: None,
            fname_transfer: Some(fname_transfer),
        }));
        assert!(!valid)
    }

    #[tokio::test]
    async fn test_mempool_prioritization() {
        let (_, _, mut mempool, mempool_tx, messages_request_tx, _shard_decision_tx, _) =
            setup(setup_config(9304));

        // Spawn mempool task
        tokio::spawn(async move {
            mempool.run().await;
        });

        let fid = 1234;
        // Cast has lower timestamp and arrives first, but onchain event is still processed first
        let onchain_event = events_factory::create_rent_event(fid, None, Some(1), false);

        let cast = messages_factory::casts::create_cast_add(
            fid,
            "hello",
            Some(to_farcaster_time(onchain_event.block_timestamp * 1000).unwrap() as u32 - 1),
            None,
        );

        mempool_tx
            .send((
                MempoolMessage::UserMessage(cast.clone()),
                MempoolSource::Local,
            ))
            .await
            .unwrap();

        mempool_tx
            .send((
                MempoolMessage::ValidatorMessage(ValidatorMessage {
                    on_chain_event: Some(onchain_event),
                    fname_transfer: None,
                }),
                MempoolSource::Local,
            ))
            .await
            .unwrap();

        // Wait for processing
        tokio::time::sleep(Duration::from_secs(1)).await;

        let pull_message = async || {
            // Setup channel to retrieve messages
            let (mempool_retrieval_tx, mempool_retrieval_rx) = oneshot::channel();

            // Query mempool for the messages
            messages_request_tx
                .send(MempoolMessagesRequest {
                    shard_id: 1,
                    max_messages_per_block: 1,
                    message_tx: mempool_retrieval_tx,
                })
                .await
                .unwrap();

            let result = mempool_retrieval_rx.await.unwrap();
            return result[0].clone();
        };

        match pull_message().await {
            MempoolMessage::UserMessage(_) => {
                panic!("Expected validator message, got user message")
            }
            MempoolMessage::ValidatorMessage(_) => {}
        }

        match pull_message().await {
            MempoolMessage::UserMessage(_) => {}
            MempoolMessage::ValidatorMessage(_) => {
                panic!("Expected user message, got validator message")
            }
        }
    }

    #[tokio::test]
    async fn test_mempool_eviction() {
        let (mut engine, _, mut mempool, mempool_tx, messages_request_tx, shard_decision_tx, _) =
            setup(setup_config(9304));
        test_helper::register_user(
            1234,
            default_signer(),
            default_custody_address(),
            &mut engine,
        )
        .await;

        // Spawn mempool task
        tokio::spawn(async move {
            mempool.run().await;
        });

        let fid = 1234;

        let cast1 = messages_factory::casts::create_cast_add(fid, "hello", None, None);
        let cast2 = messages_factory::casts::create_cast_add(fid, "world", None, None);

        let _ = mempool_tx
            .send((
                MempoolMessage::UserMessage(cast1.clone()),
                MempoolSource::Local,
            ))
            .await;
        let _ = mempool_tx
            .send((MempoolMessage::UserMessage(cast2), MempoolSource::Local))
            .await;

        // Wait for cast processing
        tokio::time::sleep(Duration::from_secs(1)).await;

        let transaction = Transaction {
            fid,
            user_messages: vec![cast1],
            system_messages: vec![],
            account_root: vec![],
        };

        let header = ShardHeader {
            height: Some(Height {
                shard_index: 1,
                block_number: 1,
            }),
            timestamp: 0,
            parent_hash: vec![],
            shard_root: vec![],
        };

        // Create fake chunk with cast1
        let chunk = ShardChunk {
            header: Some(header),
            hash: vec![],
            transactions: vec![transaction],
            commits: None,
        };

        let _ = shard_decision_tx.send(chunk);

        // Wait for chunk processing
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Setup channel to retrieve messages
        let (mempool_retrieval_tx, mempool_retrieval_rx) = oneshot::channel();

        // Query mempool for the messages
        messages_request_tx
            .send(MempoolMessagesRequest {
                shard_id: 1,
                max_messages_per_block: 2,
                message_tx: mempool_retrieval_tx,
            })
            .await
            .unwrap();

        let result = mempool_retrieval_rx.await.unwrap();
        // We expect one of the added casts to have been evicted
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].fid(), fid);
    }

    #[tokio::test]
    async fn test_mempool_gossip() {
        // Create configs with different ports
        let node1_addr = format!("/ip4/{HOST_FOR_TEST}/udp/{PORT_FOR_TEST}/quic-v1");
        let node2_port = PORT_FOR_TEST + 1;
        let node2_addr = format!("/ip4/{HOST_FOR_TEST}/udp/{node2_port}/quic-v1");
        let config1 = Config::new(node1_addr.clone(), node2_addr.clone());
        let config2 = Config::new(node2_addr.clone(), node1_addr.clone());

        let (
            _,
            mut gossip1,
            mut mempool1,
            mempool_tx1,
            _mempool_requests_tx1,
            _shard_decision_tx1,
            _,
        ) = setup(config1);
        let (
            _,
            mut gossip2,
            mut mempool2,
            mempool_tx2,
            mempool_requests_tx2,
            _shard_decision_tx1,
            mut system_rx2,
        ) = setup(config2);

        // Spawn gossip tasks
        tokio::spawn(async move {
            gossip1.start().await;
        });
        tokio::spawn(async move {
            gossip2.start().await;
        });

        // Spawn mempool tasks
        tokio::spawn(async move {
            mempool1.run().await;
        });
        tokio::spawn(async move {
            mempool2.run().await;
        });

        // Wait for connection to establish
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Create a test message
        let cast: proto::Message =
            messages_factory::casts::create_cast_add(1234, "hello", None, None);
        let cast2: proto::Message =
            messages_factory::casts::create_cast_add(3214, "hello 2", None, None);

        // Add message to mempool 1
        mempool_tx1
            .send((
                MempoolMessage::UserMessage(cast.clone()),
                MempoolSource::Local,
            ))
            .await
            .unwrap();

        // Inserting the same message twice should not be re-broadcasted
        mempool_tx1
            .send((MempoolMessage::UserMessage(cast), MempoolSource::Local))
            .await
            .unwrap();

        // Another message received via gossip should not be re-broadcasted
        mempool_tx1
            .send((MempoolMessage::UserMessage(cast2), MempoolSource::Gossip))
            .await
            .unwrap();

        // Wait for gossip
        tokio::time::sleep(Duration::from_secs(1)).await;

        let mut received_messages = 0;
        // Should be received through the system message of gossip 2
        while let Ok(msg) = system_rx2.try_recv() {
            if let SystemMessage::Mempool(mempool_message) = msg {
                // Manually forward to the mempool
                mempool_tx2.send(mempool_message).await.unwrap();
                received_messages += 1;
            }
        }
        assert_eq!(received_messages, 1);

        // Wait for the message to be processed
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Setup channel to retrieve message
        let (mempool_retrieval_tx, mempool_retrieval_rx) = oneshot::channel();

        // Query mempool 2 for the message
        mempool_requests_tx2
            .send(MempoolMessagesRequest {
                shard_id: 1,
                max_messages_per_block: 1,
                message_tx: mempool_retrieval_tx,
            })
            .await
            .unwrap();

        let result = mempool_retrieval_rx.await.unwrap();
        assert_eq!(result.len(), 1); // Only the first cast should be received
        assert_eq!(result[0].fid(), 1234);
    }
}
