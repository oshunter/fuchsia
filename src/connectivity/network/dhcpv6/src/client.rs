// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Core DHCPv6 client state transitions.

use {
    crate::protocol::{
        Dhcpv6Message, Dhcpv6MessageBuilder, Dhcpv6MessageType, Dhcpv6Option, Dhcpv6OptionCode,
    },
    packet::serialize::InnerPacketBuilder,
    rand::Rng,
    std::{default::Default, time::Duration},
    zerocopy::ByteSlice,
};

/// Initial Information-request timeout `INF_TIMEOUT` from [RFC 8415, Section 7.6].
///
/// [RFC 8415, Section 7.6]: https://tools.ietf.org/html/rfc8415#section-7.6
const INFO_REQ_TIMEOUT: Duration = Duration::from_secs(1);
/// Max Information-request timeout `INF_MAX_RT` from [RFC 8415, Section 7.6].
///
/// [RFC 8415, Section 7.6]: https://tools.ietf.org/html/rfc8415#section-7.6
const INFO_REQ_MAX_RT: Duration = Duration::from_secs(3600);
/// Default information refresh time from [RFC 8415, Section 7.6].
///
/// [RFC 8415, Section 7.6]: https://tools.ietf.org/html/rfc8415#section-7.6
const IRT_DEFAULT: Duration = Duration::from_secs(86400);

/// The max duration in seconds `std::time::Duration` supports.
///
/// NOTE: it is possible for `Duration` to be bigger by filling in the nanos field, but this value
/// is good enough for the purpose of this crate.
const MAX_DURATION: Duration = Duration::from_secs(std::u64::MAX);

/// Calculates retransmission timeout based on formulas defined in [RFC 8415, Section 15].
/// A zero `prev_retrans_timeout` indicates this is the first transmission, so
/// `initial_retrans_timeout` will be used.
///
/// Relevant formulas from [RFC 8415, Section 15]:
///
/// ```text
/// RT      Retransmission timeout
/// IRT     Initial retransmission time
/// MRT     Maximum retransmission time
/// RAND    Randomization factor
///
/// RT for the first message transmission is based on IRT:
///
///     RT = IRT + RAND*IRT
///
/// RT for each subsequent message transmission is based on the previous value of RT:
///
///     RT = 2*RTprev + RAND*RTprev
///
/// MRT specifies an upper bound on the value of RT (disregarding the randomization added by
/// the use of RAND).  If MRT has a value of 0, there is no upper limit on the value of RT.
/// Otherwise:
///
///     if (RT > MRT)
///         RT = MRT + RAND*MRT
/// ```
///
/// [RFC 8415, Section 15]: https://tools.ietf.org/html/rfc8415#section-15
fn retransmission_timeout<R: Rng>(
    prev_retrans_timeout: Duration,
    initial_retrans_timeout: Duration,
    max_retrans_timeout: Duration,
    rng: &mut R,
) -> Duration {
    let rand = rng.gen_range(-0.1, 0.1);

    let next_rt = if prev_retrans_timeout.as_nanos() == 0 {
        let irt = initial_retrans_timeout.as_secs_f64();
        irt + rand * irt
    } else {
        let rt = prev_retrans_timeout.as_secs_f64();
        2. * rt + rand * rt
    };

    if max_retrans_timeout.as_nanos() == 0 || next_rt < max_retrans_timeout.as_secs_f64() {
        clipped_duration(next_rt)
    } else {
        let mrt = max_retrans_timeout.as_secs_f64();
        clipped_duration(mrt + rand * mrt)
    }
}

/// Clips overflow and returns a duration using the input seconds.
fn clipped_duration(secs: f64) -> Duration {
    if secs <= 0. {
        Duration::from_nanos(0)
    } else if secs >= MAX_DURATION.as_secs_f64() {
        MAX_DURATION
    } else {
        Duration::from_secs_f64(secs)
    }
}

/// Identifies what event should be triggered when a timer fires.
#[derive(Debug, PartialEq, Eq, Hash)]
enum Dhcpv6ClientTimerType {
    Retransmission,
    Refresh,
}

/// Possible actions that need to be taken for a state transition to happen successfully.
#[derive(Debug)]
enum Action {
    SendMessage(Vec<u8>),
    ScheduleTimer(Dhcpv6ClientTimerType, Duration),
    CancelTimer(Dhcpv6ClientTimerType),
}

type Actions = Vec<Action>;

/// Holds data and provides methods for handling state transitions from information requesting
/// state.
#[derive(Debug, Default)]
struct InformationRequesting {
    retrans_timeout: Duration,
}

impl InformationRequesting {
    /// Starts in information requesting state following [RFC 8415, Section 18.2.6].
    ///
    /// [RFC 8415, Section 18.2.6]: https://tools.ietf.org/html/rfc8415#section-18.2.6
    fn start<R: Rng>(
        transaction_id: [u8; 3],
        options_to_request: &[Dhcpv6OptionCode],
        rng: &mut R,
    ) -> Transition {
        let info_req: Self = Default::default();
        info_req.send_and_schedule_retransmission(transaction_id, options_to_request, rng)
    }

    /// Calculates timeout for retransmitting information requests using parameters specified in
    /// [RFC 8415, Section 18.2.6].
    ///
    /// [RFC 8415, Section 18.2.6]: https://tools.ietf.org/html/rfc8415#section-18.2.6
    fn retransmission_timeout<R: Rng>(&self, rng: &mut R) -> Duration {
        retransmission_timeout(self.retrans_timeout, INFO_REQ_TIMEOUT, INFO_REQ_MAX_RT, rng)
    }

    /// A helper function that returns a transition back to `InformationRequesting`, with actions
    /// to send an information request and schedules retransmission.
    fn send_and_schedule_retransmission<R: Rng>(
        self,
        transaction_id: [u8; 3],
        options_to_request: &[Dhcpv6OptionCode],
        rng: &mut R,
    ) -> Transition {
        let options = if options_to_request.is_empty() {
            Vec::new()
        } else {
            vec![Dhcpv6Option::Oro(options_to_request.to_vec())]
        };

        let builder = Dhcpv6MessageBuilder::new(
            Dhcpv6MessageType::InformationRequest,
            transaction_id,
            &options,
        );
        let mut buf = vec![0; builder.bytes_len()];
        builder.serialize(&mut buf);

        let retrans_timeout = self.retransmission_timeout(rng);

        (
            Dhcpv6ClientState::InformationRequesting(InformationRequesting { retrans_timeout }),
            vec![
                Action::SendMessage(buf),
                Action::ScheduleTimer(Dhcpv6ClientTimerType::Retransmission, retrans_timeout),
            ],
        )
    }

    /// Retransmits information request.
    fn retransmission_timer_expired<R: Rng>(
        self,
        transaction_id: [u8; 3],
        options_to_request: &[Dhcpv6OptionCode],
        rng: &mut R,
    ) -> Transition {
        self.send_and_schedule_retransmission(transaction_id, options_to_request, rng)
    }

    /// Handles reply to information requests based on [RFC 8415, Section 18.2.10.4].
    ///
    /// [RFC 8415, Section 18.2.10.4]: https://tools.ietf.org/html/rfc8415#section-18.2.10.4
    fn reply_message_received<B: ByteSlice>(self, msg: Dhcpv6Message<'_, B>) -> Transition {
        let actions_from_options = msg.options.iter().filter_map(|opt| match opt {
            Dhcpv6Option::InformationRefreshTime(refresh_time) => Some(Action::ScheduleTimer(
                Dhcpv6ClientTimerType::Refresh,
                Duration::from_secs(u64::from(refresh_time)),
            )),
            // TODO(jayzhuang): emit more actions for other options received.
            _ => None,
        });

        // Use default refresh timer if the response didn't include one.
        let maybe_schedule_default_timer = if msg.options.iter().any(|opt| match opt {
            Dhcpv6Option::InformationRefreshTime(_) => true,
            _ => false,
        }) {
            None
        } else {
            Some(Action::ScheduleTimer(Dhcpv6ClientTimerType::Refresh, IRT_DEFAULT))
        };

        let actions = vec![Action::CancelTimer(Dhcpv6ClientTimerType::Retransmission)]
            .into_iter()
            .chain(maybe_schedule_default_timer)
            .chain(actions_from_options)
            .collect::<Vec<_>>();

        (Dhcpv6ClientState::InformationReceived(InformationReceived {}), actions)
    }
}

/// Provides methods for handling state transitions from information received state.
#[derive(Debug)]
struct InformationReceived {}

impl InformationReceived {
    /// Refreshes information by starting another round of information request.
    fn refresh_timer_expired<R: Rng>(
        self,
        transaction_id: [u8; 3],
        options_to_request: &[Dhcpv6OptionCode],
        rng: &mut R,
    ) -> Transition {
        InformationRequesting::start(transaction_id, options_to_request, rng)
    }
}

/// All possible states of a DHCPv6 client.
///
/// States not found in this enum are not supported yet.
#[derive(Debug)]
enum Dhcpv6ClientState {
    /// Creating and (re)transmitting an information request, and waiting for a reply.
    InformationRequesting(InformationRequesting),
    /// Client is waiting to refresh, after receiving a valid reply to a previous information
    /// request.
    InformationReceived(InformationReceived),
}

/// Defines the next state, and the actions the client should take to transition to that state.
type Transition = (Dhcpv6ClientState, Actions);

impl Dhcpv6ClientState {
    /// Dispatches reply message received event based on the current state (self).
    ///
    /// Consumes `self` and returns the transition the client should take.
    fn reply_message_received<B: ByteSlice>(self, msg: Dhcpv6Message<'_, B>) -> Transition {
        match self {
            Dhcpv6ClientState::InformationRequesting(s) => s.reply_message_received(msg),
            state => (state, vec![]),
        }
    }

    /// Dispatches retransmission timer expired event based on the current state (self).
    ///
    /// Consumes `self` and returns the transition the client should take.
    fn retransmission_timer_expired<R: Rng>(
        self,
        transaction_id: [u8; 3],
        options_to_request: &[Dhcpv6OptionCode],
        rng: &mut R,
    ) -> Transition {
        match self {
            Dhcpv6ClientState::InformationRequesting(s) => {
                s.retransmission_timer_expired(transaction_id, options_to_request, rng)
            }
            state => (state, vec![]),
        }
    }

    /// Dispatches refresh timer expired event based on the current state (self).
    ///
    /// Consumes `self` and returns the transition the client should take.
    fn refresh_timer_expired<R: Rng>(
        self,
        transaction_id: [u8; 3],
        options_to_request: &[Dhcpv6OptionCode],
        rng: &mut R,
    ) -> Transition {
        match self {
            Dhcpv6ClientState::InformationReceived(s) => {
                s.refresh_timer_expired(transaction_id, options_to_request, rng)
            }
            state => (state, vec![]),
        }
    }
}

/// The DHCPv6 core state machine.
///
/// This struct maintains the state machine for a DHCPv6 client, and expects an imperative shell to
/// drive it by taking necessary actions (e.g. send packets, schedule timers, etc.) and dispatch
/// events (e.g. packets received, timer expired, etc.). All the functions provided by this struct
/// are pure-functional. All state transition functions return a list of actions that the
/// imperative shell should take to complete the transition.
struct Dhcpv6ClientStateMachine<R: Rng> {
    transaction_id: [u8; 3],
    options_to_request: Vec<Dhcpv6OptionCode>,
    state: Option<Dhcpv6ClientState>,

    rng: R,
}

/// Creates a transaction id that can be used by the client as defined in [RFC 8415, Section 16.1].
///
/// [RFC 8415, Section 16.1]: https://tools.ietf.org/html/rfc8415#section-16.1
fn transaction_id<R: Rng>(rng: &mut R) -> [u8; 3] {
    let mut id = [0u8; 3];
    for i in 0..id.len() {
        id[i] = rng.gen();
    }
    id
}

impl<R: Rng> Dhcpv6ClientStateMachine<R> {
    /// Starts the client to send information requests and respond to replies. The client will
    /// operate in the Stateless DHCP model defined in [RFC 8415, Section 6.1].
    ///
    /// [RFC 8415, Section 6.1]: https://tools.ietf.org/html/rfc8415#section-6.1
    fn start_information_request(
        options_to_request: Vec<Dhcpv6OptionCode>,
        mut rng: R,
    ) -> (Self, Actions) {
        let transaction_id = transaction_id(&mut rng);
        let (state, actions) =
            InformationRequesting::start(transaction_id, &options_to_request, &mut rng);
        (Self { state: Some(state), transaction_id, options_to_request, rng }, actions)
    }

    /// Handles a timeout event, dispatches based on timeout type.
    ///
    /// # Panics
    ///
    /// `handle_timeout` panics if current state is None.
    fn handle_timeout(&mut self, timeout_type: Dhcpv6ClientTimerType) -> Actions {
        let state = self.state.take().expect("state should not be empty");
        let (new_state, actions) = match timeout_type {
            Dhcpv6ClientTimerType::Retransmission => state.retransmission_timer_expired(
                self.transaction_id,
                &self.options_to_request,
                &mut self.rng,
            ),
            Dhcpv6ClientTimerType::Refresh => state.refresh_timer_expired(
                self.transaction_id,
                &self.options_to_request,
                &mut self.rng,
            ),
        };
        self.state = Some(new_state);
        actions
    }

    /// Handles a received DHCPv6 message.
    ///
    /// # Panics
    ///
    /// `handle_reply` panics if current state is None.
    fn handle_message_receive<B: ByteSlice>(&mut self, msg: Dhcpv6Message<'_, B>) -> Actions {
        if msg.transaction_id != &self.transaction_id {
            Vec::new() // Ignore messages for other clients.
        } else {
            match msg.msg_type {
                Dhcpv6MessageType::Reply => {
                    let (new_state, actions) = self
                        .state
                        .take()
                        .expect("state should not be empty")
                        .reply_message_received(msg);
                    self.state = Some(new_state);
                    actions
                }
                Dhcpv6MessageType::Advertise => {
                    // TODO(jayzhuang): support Advertise messages when needed.
                    // https://tools.ietf.org/html/rfc8415#section-18.2.9
                    Vec::new()
                }
                Dhcpv6MessageType::Reconfigure => {
                    // TODO(jayzhuang): support Reconfigure messages when needed.
                    // https://tools.ietf.org/html/rfc8415#section-18.2.11
                    Vec::new()
                }
                _ => {
                    // Ignore unexpected message types.
                    Vec::new()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, packet::ParsablePacket, rand::rngs::mock::StepRng};

    fn validate_send_message(
        action: &Action,
        want_msg_type: Dhcpv6MessageType,
        want_transaction_id: [u8; 3],
        want_options: Vec<Dhcpv6Option<'_>>,
    ) {
        match action {
            Action::SendMessage(buf) => {
                let mut buf = &buf[..]; // Implements BufferView.
                let msg = Dhcpv6Message::parse(&mut buf, ()).expect("failed to parse test buffer");
                assert_eq!(msg.msg_type, want_msg_type);
                assert_eq!(msg.transaction_id, &want_transaction_id);
                assert_eq!(msg.options.iter().collect::<Vec<_>>(), want_options);
            }
            action => panic!("unexpected action {:?}, want SendMessage", action),
        };
    }

    fn validate_schedule_timer(
        action: &Action,
        want_timer_type: Dhcpv6ClientTimerType,
        want_duration: Duration,
    ) {
        match action {
            Action::ScheduleTimer(timer_type, t) => {
                assert_eq!(*timer_type, want_timer_type);
                assert_eq!(*t, want_duration);
            }
            action => panic!("unexpected action {:?}, want ScheduleTimer", action),
        }
    }

    #[test]
    fn test_information_request_and_reply() {
        // Try to start information request with different list of requested options.
        for options in vec![
            Vec::new(),
            vec![Dhcpv6OptionCode::DnsServers],
            vec![Dhcpv6OptionCode::DnsServers, Dhcpv6OptionCode::DomainList],
        ] {
            let (mut client, actions) = Dhcpv6ClientStateMachine::start_information_request(
                options.clone(),
                StepRng::new(std::u64::MAX / 2, 0),
            );

            match client.state.as_ref().expect("state should not be empty") {
                Dhcpv6ClientState::InformationRequesting(_) => {}
                state => panic!("unexpected state {:?}, want InformationRequesting", state),
            }

            // Start of information requesting should send a information request and schedule a
            // retransmission timer.
            assert_eq!(actions.len(), 2);
            let want_options = if options.is_empty() {
                Vec::new()
            } else {
                vec![Dhcpv6Option::Oro(options.clone())]
            };
            validate_send_message(
                &actions[0],
                Dhcpv6MessageType::InformationRequest,
                client.transaction_id,
                want_options,
            );
            validate_schedule_timer(
                &actions[1],
                Dhcpv6ClientTimerType::Retransmission,
                INFO_REQ_TIMEOUT,
            );

            let test_dhcp_refresh_time = 42u32;
            let options = [Dhcpv6Option::InformationRefreshTime(test_dhcp_refresh_time)];
            let builder = Dhcpv6MessageBuilder::new(
                Dhcpv6MessageType::Reply,
                client.transaction_id,
                &options,
            );
            let mut buf = vec![0; builder.bytes_len()];
            builder.serialize(&mut buf);
            let mut buf = &buf[..]; // Implements BufferView.
            let msg = Dhcpv6Message::parse(&mut buf, ()).expect("failed to parse test buffer");

            let actions = client.handle_message_receive(msg);

            match client.state.as_ref().expect("state should not be empty") {
                Dhcpv6ClientState::InformationReceived(_) => {}
                state => panic!("unexpected state {:?}, want InformationReceived", state),
            };

            // Upon receiving a valid reply, client should set up for refresh based on the reply.
            assert_eq!(actions.len(), 2);
            match &actions[0] {
                Action::CancelTimer(timer_type) => {
                    assert_eq!(*timer_type, Dhcpv6ClientTimerType::Retransmission);
                }
                action => panic!("unexpected action {:?}, want CancelTimer", action),
            };
            match &actions[1] {
                Action::ScheduleTimer(timer_type, t) => {
                    assert_eq!(*timer_type, Dhcpv6ClientTimerType::Refresh);
                    assert_eq!(t.as_secs(), u64::from(test_dhcp_refresh_time));
                }
                action => panic!("unexpected action {:?}, want ScheduleTimer", action),
            }
        }
    }

    #[test]
    fn test_unexpected_messages_are_ignored() {
        let (mut client, _) = Dhcpv6ClientStateMachine::start_information_request(
            Vec::new(),
            StepRng::new(std::u64::MAX / 2, 0),
        );
        client.transaction_id = [1, 2, 3];

        let builder = Dhcpv6MessageBuilder::new(
            Dhcpv6MessageType::Reply,
            // Transaction ID is different from the client's.
            [4, 5, 6],
            &[],
        );
        let mut buf = vec![0; builder.bytes_len()];
        builder.serialize(&mut buf);
        let mut buf = &buf[..]; // Implements BufferView.
        let msg = Dhcpv6Message::parse(&mut buf, ()).expect("failed to parse test buffer");

        assert!(client.handle_message_receive(msg).is_empty());

        // Messages with unsupported/unexpected types are discarded.
        for msg_type in vec![
            Dhcpv6MessageType::Solicit,
            Dhcpv6MessageType::Advertise,
            Dhcpv6MessageType::Request,
            Dhcpv6MessageType::Confirm,
            Dhcpv6MessageType::Renew,
            Dhcpv6MessageType::Rebind,
            Dhcpv6MessageType::Release,
            Dhcpv6MessageType::Decline,
            Dhcpv6MessageType::Reconfigure,
            Dhcpv6MessageType::InformationRequest,
            Dhcpv6MessageType::RelayForw,
            Dhcpv6MessageType::RelayRepl,
        ] {
            let builder = Dhcpv6MessageBuilder::new(msg_type, client.transaction_id, &[]);
            let mut buf = vec![0; builder.bytes_len()];
            builder.serialize(&mut buf);
            let mut buf = &buf[..]; // Implements BufferView.
            let msg = Dhcpv6Message::parse(&mut buf, ()).expect("failed to parse test buffer");

            assert!(client.handle_message_receive(msg).is_empty());
        }
    }

    #[test]
    fn test_unexpected_events_are_ignored() {
        let (mut client, _) = Dhcpv6ClientStateMachine::start_information_request(
            Vec::new(),
            StepRng::new(std::u64::MAX / 2, 0),
        );

        // The client expects either a reply or retransmission timeout in the current state.
        client.handle_timeout(Dhcpv6ClientTimerType::Refresh);
        match client.state.as_ref().expect("state should not be empty") {
            Dhcpv6ClientState::InformationRequesting(_) => {}
            state => panic!("unexpected state {:?}, want InformationRequesting", state),
        };

        let builder =
            Dhcpv6MessageBuilder::new(Dhcpv6MessageType::Reply, client.transaction_id, &[]);
        let mut buf = vec![0; builder.bytes_len()];
        builder.serialize(&mut buf);
        let mut buf = &buf[..]; // Implements BufferView.
        let msg = Dhcpv6Message::parse(&mut buf, ()).expect("failed to parse test buffer");
        // Transition to InformationReceived state.
        client.handle_message_receive(msg);

        match client.state.as_ref().expect("state should not be empty") {
            Dhcpv6ClientState::InformationReceived(_) => {}
            state => panic!("unexpected state {:?}, want InformationReceived", state),
        };

        let mut buf = vec![0; builder.bytes_len()];
        builder.serialize(&mut buf);
        let mut buf = &buf[..]; // Implements BufferView.
        let msg = Dhcpv6Message::parse(&mut buf, ()).expect("failed to parse test buffer");
        client.handle_message_receive(msg);
        match client.state.as_ref().expect("state should not be empty") {
            Dhcpv6ClientState::InformationReceived(_) => {}
            state => panic!("unexpected state {:?}, want InformationReceived", state),
        };

        client.handle_timeout(Dhcpv6ClientTimerType::Retransmission);
        match client.state.as_ref().expect("state should not be empty") {
            Dhcpv6ClientState::InformationReceived(_) => {}
            state => panic!("unexpected state {:?}, want InformationReceived", state),
        };
    }

    #[test]
    fn test_information_request_retransmission() {
        let (mut client, actions) = Dhcpv6ClientStateMachine::start_information_request(
            Vec::new(),
            StepRng::new(std::u64::MAX / 2, 0),
        );
        assert_eq!(actions.len(), 2);
        validate_schedule_timer(
            &actions[1],
            Dhcpv6ClientTimerType::Retransmission,
            INFO_REQ_TIMEOUT,
        );

        let actions = client.handle_timeout(Dhcpv6ClientTimerType::Retransmission);
        assert_eq!(actions.len(), 2);
        // Following exponential backoff defined in https://tools.ietf.org/html/rfc8415#section-15.
        validate_schedule_timer(
            &actions[1],
            Dhcpv6ClientTimerType::Retransmission,
            2 * INFO_REQ_TIMEOUT,
        );
    }

    #[test]
    fn test_information_request_refresh() {
        let (mut client, _) = Dhcpv6ClientStateMachine::start_information_request(
            Vec::new(),
            StepRng::new(std::u64::MAX / 2, 0),
        );

        let builder =
            Dhcpv6MessageBuilder::new(Dhcpv6MessageType::Reply, client.transaction_id, &[]);
        let mut buf = vec![0; builder.bytes_len()];
        builder.serialize(&mut buf);
        let mut buf = &buf[..]; // Implements BufferView.
        let msg = Dhcpv6Message::parse(&mut buf, ()).expect("failed to parse test buffer");

        // Transition to InformationReceived state.
        client.handle_message_receive(msg);

        // Refresh should start another round of information request.
        let actions = client.handle_timeout(Dhcpv6ClientTimerType::Refresh);
        validate_send_message(
            &actions[0],
            Dhcpv6MessageType::InformationRequest,
            client.transaction_id,
            Vec::new(),
        );
        validate_schedule_timer(
            &actions[1],
            Dhcpv6ClientTimerType::Retransmission,
            INFO_REQ_TIMEOUT,
        );
    }

    // NOTE: All comparisons are done on millisecond, so this test is not affected by precision
    // loss from floating point arithmetic.
    #[test]
    fn test_retransmission_timeout() {
        let mut rng = StepRng::new(std::u64::MAX / 2, 0);

        let initial_rt = Duration::from_secs(1);
        let max_rt = Duration::from_secs(100);

        // Start with initial timeout if previous timeout is zero.
        let t = retransmission_timeout(Duration::from_nanos(0), initial_rt, max_rt, &mut rng);
        assert_eq!(t.as_millis(), initial_rt.as_millis());

        // Use previous timeout when it's not zero and apply the formula.
        let t = retransmission_timeout(Duration::from_secs(10), initial_rt, max_rt, &mut rng);
        assert_eq!(t, Duration::from_secs(20));

        // Cap at max timeout.
        let t = retransmission_timeout(100 * max_rt, initial_rt, max_rt, &mut rng);
        assert_eq!(t.as_millis(), max_rt.as_millis());
        let t = retransmission_timeout(MAX_DURATION, initial_rt, max_rt, &mut rng);
        assert_eq!(t.as_millis(), max_rt.as_millis());
        // Zero max means no cap.
        let t = retransmission_timeout(100 * max_rt, initial_rt, Duration::from_nanos(0), &mut rng);
        assert_eq!(t.as_millis(), (200 * max_rt).as_millis());
        // Overflow durations are clipped.
        let t = retransmission_timeout(MAX_DURATION, initial_rt, Duration::from_nanos(0), &mut rng);
        assert_eq!(t.as_millis(), MAX_DURATION.as_millis());

        // Steps through the range with deterministic randomness, 20% at a time.
        let mut rng = StepRng::new(0, std::u64::MAX / 5);
        [
            (Duration::from_millis(10000), 19000),
            (Duration::from_millis(10000), 19400),
            (Duration::from_millis(10000), 19800),
            (Duration::from_millis(10000), 20200),
            (Duration::from_millis(10000), 20600),
            (Duration::from_millis(10000), 21000),
            (Duration::from_millis(10000), 19400),
            // Cap at max timeout with randomness.
            (100 * max_rt, 98000),
            (100 * max_rt, 102000),
            (100 * max_rt, 106000),
            (100 * max_rt, 110000),
            (100 * max_rt, 94000),
            (100 * max_rt, 98000),
        ]
        .iter()
        .for_each(|(rt, want_ms)| {
            let t = retransmission_timeout(*rt, initial_rt, max_rt, &mut rng);
            assert_eq!(t.as_millis(), *want_ms);
        });
    }
}