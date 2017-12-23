use messenger::*;
use algo::*;
use super::Instance;
use state::*;
use config::*;

pub trait ReplicatedState {
    /// Apply a value to the state machine
    fn apply_value(&mut self, instance: Instance, value: Value);

    // TODO: need log semantics
    /// Snapshots the value
    fn snapshot(&self, instance: Instance) -> Option<Value>;
}

pub struct MultiPaxos<R : ReplicatedState, M: Messenger> {
    state_machine: R,
    messenger: M,
    state_handler: StateHandler,

    instance: Instance,
    paxos: PaxosInstance,

    config: Configuration,

    // message that is being sent out for quorum. the
    // retransmission logic will prediodically resend
    // until there is quorum
    retransmit_msg: Option<ProposerMsg>,
}

impl<R: ReplicatedState, M: Messenger> MultiPaxos<R, M> {

    /// Creates a new multi-paxos machine
    pub fn new(messenger: M, mut state_machine: R, config: Configuration) -> MultiPaxos<R, M> {
        let mut state_handler = StateHandler::new();

        let state = state_handler.load().unwrap_or_default();
        let paxos = PaxosInstance::new(
            config.current(),
            config.quorum_size(),
            state.promised,
            state.accepted,
        );

        if let Some(v) = state.current_value.clone() {
            state_machine.apply_value(state.instance, v);
        }

        MultiPaxos {
            state_machine,
            messenger,
            state_handler,
            instance: state.instance,
            paxos,
            config,
            retransmit_msg: None,
        }
    }

    fn advance_instance(&mut self, instance: Instance, value: Value) {
        self.state_machine.apply_value(instance, value.clone());

        let new_inst = instance + 1;
        info!("Starting instance {}", new_inst);
        self.state_handler.persist(State {
            instance: new_inst,
            current_value: Some(value),
            promised: None,
            accepted: None,
        });
        self.retransmit_msg = None;
        self.paxos =
            PaxosInstance::new(self.config.current(), self.config.quorum_size(), None, None);
    }

    /// Broadcasts PREPARE messages to all peers
    fn send_prepare(&mut self, prepare: &Prepare) {
        let peers = self.config.peers();
        for peer in peers.into_iter() {
            self.messenger.send_prepare(peer, self.instance, prepare.1);
        }
    }

    /// Broadcasts ACCEPT messages to all peers
    fn send_accept(&mut self, accept: &Accept) {
        let peers = self.config.peers();
        for peer in peers.into_iter() {
            self.messenger
                .send_accept(peer, self.instance, accept.1, accept.2.clone());
        }
    }

    /// Broadcasts ACCEPTED messages to all peers
    fn send_accepted(&mut self, accepted: &Accepted) {
        let peers = self.config.peers();
        for peer in peers.into_iter() {
            self.messenger
                .send_accepted(peer, self.instance, accepted.1, accepted.2.clone());
        }
    }

    /*fn propose_update(&mut self, value: Value) -> Poll<Instance> {
        match self.paxos.propose_value(value) {
            Some(ProposerMsg::Prepare(prepare)) => {
                info!("Starting Phase 1a with proposed value");
                self.send_prepare(&prepare);
                self.retransmit_msg = Some(ProposerMsg::Prepare(prepare));
            }
            Some(ProposerMsg::Accept(accept)) => {
                info!("Starting Phase 2a with proposed value");
                self.send_accept(&accept);
                self.retransmit_msg = Some(ProposerMsg::Accept(accept));
            }
            None => {
                warn!("Alrady have a value during proposal phases");
            }
        }
        Poll::Schedule(self.instance)
    }

    fn poll_retransmit(&mut self, instance: Instance) -> Poll<Instance> {
        if instance != self.instance {
            return Poll::Cancel;
        }


        // resend prepare messages to peers
        let msg = self.retransmit_msg.take();
        let poll = match msg {
            Some(ProposerMsg::Prepare(ref v)) => {
                debug!("Retransmitting {:?} to followers", v);
                self.send_prepare(v);
                Poll::Schedule(self.instance)
            }
            Some(ProposerMsg::Accept(ref v)) => {
                debug!("Retransmitting {:?} to followers", v);
                self.send_accept(v);
                Poll::Schedule(self.instance)
            }
            None => Poll::Cancel,
        };

        self.retransmit_msg = msg;
        poll
    }

    fn poll_restart_prepare(&mut self, instance: Instance) -> Poll<Instance> {
        if instance != self.instance {
            return Poll::Cancel;
        }

        let prepare = self.paxos.prepare();
        info!("Restarting Phase 1 with {:?}", prepare.1);
        self.send_prepare(&prepare);
        self.retransmit_msg = Some(ProposerMsg::Prepare(prepare));
        Poll::Schedule(instance)
    }

    fn poll_syncronization(&mut self) -> Poll<()> {
        if let Some(node) = self.config.random_peer() {
            debug!("Sending SYNC request");
            self.messenger.send_sync(node, self.instance);
        }

        Poll::Schedule(())
    }*/
}

impl<R: ReplicatedState, M: Messenger> Handler for MultiPaxos<R, M> {
    fn on_prepare(&mut self, peer: NodeId, inst: Instance, proposal: Ballot) {
        // ignore previous or future instances
        if self.instance != inst {
            return;
        }

        match self.paxos.receive_prepare(Prepare(peer, proposal)) {
            Ok(Promise(_, ballot, last_accepted)) => {
                self.state_handler.persist(State {
                    instance: self.instance,
                    current_value: self.state_machine.snapshot(inst).clone(),
                    promised: Some(ballot),
                    accepted: last_accepted.clone(),
                });

                self.messenger
                    .send_promise(peer, self.instance, ballot, last_accepted);
            }
            Err(Reject(_, ballot, opposing_ballot)) => {
                self.messenger
                    .send_reject(peer, self.instance, ballot, opposing_ballot);
            }
        }
    }

    fn on_promise(
        &mut self,
        peer: NodeId,
        inst: Instance,
        proposal: Ballot,
        last_accepted: Option<(Ballot, Value)>,
    ) {
        // ignore previous or future instances
        if self.instance != inst {
            return;
        }

        let promise = Promise(peer, proposal, last_accepted);
        if let Some(accept) = self.paxos.receive_promise(promise) {
            self.send_accept(&accept);
            self.retransmit_msg = Some(ProposerMsg::Accept(accept));
        }
    }

    fn on_reject(&mut self, peer: NodeId, inst: Instance, proposal: Ballot, promised: Ballot) {
        // ignore previous or future instances
        if self.instance != inst {
            return;
        }

        // go back to phase 1 when a quorum of REJECT has been received
        let prepare = self.paxos.receive_reject(Reject(peer, proposal, promised));
        if let Some(prepare) = prepare {
            self.send_prepare(&prepare);
            self.retransmit_msg = Some(ProposerMsg::Prepare(prepare));
        }
    }

    fn on_accept(&mut self, peer: NodeId, inst: Instance, proposal: Ballot, value: Value) {
        // ignore previous or future instances
        if self.instance != inst {
            return;
        }

        match self.paxos.receive_accept(Accept(peer, proposal, value)) {
            Ok(accepted @ Accepted(..)) => {
                self.state_handler.persist(State {
                    instance: self.instance,
                    current_value: self.state_machine.snapshot(inst).clone(),
                    promised: Some(accepted.1),
                    accepted: Some((accepted.1, accepted.2.clone())),
                });

                self.send_accepted(&accepted);
            }
            Err(Reject(_, ballot, opposing_ballot)) => {
                self.messenger
                    .send_reject(peer, self.instance, ballot, opposing_ballot);
            }
        }
    }

    fn on_accepted(&mut self, peer: NodeId, inst: Instance, proposal: Ballot, value: Value) {
        // ignore previous or future instances
        if self.instance != inst {
            return;
        }

        let resol = self.paxos.receive_accepted(Accepted(peer, proposal, value));

        // if there is quorum, we can advance to the next instance
        if let Some(Resolution(_, _, value)) = resol {
            self.advance_instance(inst, value);
        }
    }

    fn on_sync(&mut self, peer: NodeId, inst: Instance) {
        if self.instance <= inst {
            return;
        }

        // receives SYNC request from a peer to get the present value
        // if the instance known to the peer preceeds the current
        // known instance's value
        //
        // Why is this `self.instance - 1`?
        //
        // The catchup will send the current instance (which may be in-flight)
        // and the value from the last instance.
        if let Some(v) = self.state_machine.snapshot(self.instance - 1) {
            self.messenger.send_catchup(peer, self.instance - 1, v);
        }
    }

    fn on_catchup(&mut self, peer: NodeId, inst: Instance, current: Value) {
        // only accept a catchup value if it is greater than
        // the current instance known to this node
        if inst > self.instance {
            self.advance_instance(inst, current);
        }
    }
}
