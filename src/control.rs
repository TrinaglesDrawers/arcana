use {
    crate::{
        event::{DeviceEvent, DeviceId, Event},
        funnel::Funnel,
        resources::Res,
        // session::{ClientSession, NetId},
    },
    hecs::{Entity, World},
    std::collections::{
        hash_map::{Entry, HashMap},
        VecDeque,
    },
};

/// Device is already associated with a controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Device ({device_id:?}) is already associated with a controller")]
pub struct DeviceUsed {
    device_id: DeviceId,
}

/// Result of `InputController::control` method
pub enum ControlResult {
    /// Event was consumed by the controller.
    /// It should not be propagated further.
    Consumed,

    /// Event ignored.
    /// It should be propagated further.
    Ignored,

    /// Controller detached and should be removed.
    /// Event should be propagated further.
    ControlLost,
}

/// An input controller.
/// Receives device events from `Control` hub.
pub trait InputController: Send + 'static {
    /// Translates device event into controls.
    fn control(&mut self, event: DeviceEvent, res: &mut Res, world: &mut World) -> ControlResult;
}

/// Collection of controllers.
pub struct Control {
    /// Controllers bound to specific devices.
    devices: HashMap<DeviceId, Box<dyn InputController>>,

    /// Global controller that receives all events unhandled by device specific controllers.
    global: slab::Slab<Box<dyn InputController>>,
}

/// Identifier of the controller set in global slot.
/// See [`Control::add_global_controller`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct GlobalControllerId {
    idx: usize,
}

impl Control {
    /// Returns empty collection of controllers
    pub fn new() -> Control {
        Control {
            devices: HashMap::new(),
            global: slab::Slab::new(),
        }
    }

    /// Assign global controller.
    pub fn add_global_controller(
        &mut self,
        controller: impl InputController,
    ) -> GlobalControllerId {
        let idx = self.global.insert(Box::new(controller));
        GlobalControllerId { idx }
    }

    /// Assign global controller to specific device.
    pub fn set_device_control(
        &mut self,
        device_id: DeviceId,
        controller: impl InputController,
    ) -> Result<(), DeviceUsed> {
        match self.devices.entry(device_id) {
            Entry::Occupied(_) => Err(DeviceUsed { device_id }),
            Entry::Vacant(entry) => {
                entry.insert(Box::new(controller));
                Ok(())
            }
        }
    }
}

impl Funnel<Event> for Control {
    fn filter(&mut self, res: &mut Res, world: &mut World, event: Event) -> Option<Event> {
        match event {
            Event::DeviceEvent { device_id, event } => {
                let mut event_opt = match self.devices.get_mut(&device_id) {
                    Some(controller) => match controller.control(event.clone(), res, world) {
                        ControlResult::ControlLost => {
                            self.devices.remove(&device_id);
                            Some(event)
                        }
                        ControlResult::Consumed => None,
                        ControlResult::Ignored => Some(event),
                    },
                    None => Some(event),
                };

                for idx in 0..self.global.len() {
                    if let Some(event) = event_opt.take() {
                        if let Some(controller) = self.global.get_mut(idx) {
                            match controller.control(event.clone(), res, world) {
                                ControlResult::ControlLost => {
                                    self.global.remove(idx);
                                    event_opt = Some(event);
                                }
                                ControlResult::Consumed => {}
                                ControlResult::Ignored => event_opt = Some(event),
                            }
                        }
                    } else {
                        break;
                    }
                }

                event_opt.map(|event| Event::DeviceEvent { device_id, event })
            }
            _ => Some(event),
        }
    }
}

/// A queue of commands.
/// It should be used as a component on controlled entity.
pub struct CommandQueue<T> {
    commands: VecDeque<T>,
}

impl<T> CommandQueue<T> {
    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.commands.drain(..)
    }
}

/// Translates device events into commands and
pub trait InputCommander {
    type Command;

    fn translate(&mut self, event: DeviceEvent) -> Option<Self::Command>;
}

/// Error that can occur when assuming control over an entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AssumeControlError {
    /// Failed to assume control of non-existing entity.
    #[error("Failed to assume control of non-existing entity ({entity:?})")]
    NoSuchEntity { entity: Entity },

    /// Entity is already controlled
    #[error("Entity ({entity:?}) is already controlled")]
    AlreadyControlled { entity: Entity },
}

/// Marker component. Marks that entity is being controlled.
pub struct Controlled {
    // Forbid construction outside of this module.
    __: (),
}

const CONTROLLED: Controlled = Controlled { __: () };

/// A kind of [`InputController`]s that yield commands and sends them to a command queue of an entity.
pub struct EntityController<T> {
    commander: T,
    entity: Entity,
}

impl<T> EntityController<T>
where
    T: InputCommander,
    T::Command: Send + Sync + 'static,
{
    pub fn assume_control(
        commander: T,
        queue_cap: usize,
        entity: Entity,
        world: &mut World,
    ) -> Result<Self, AssumeControlError> {
        match world.query_one_mut::<&Controlled>(entity).is_ok() {
            true => Err(AssumeControlError::AlreadyControlled { entity }),
            false => {
                world
                    .insert(
                        entity,
                        (
                            CONTROLLED,
                            CommandQueue::<T::Command> {
                                commands: VecDeque::with_capacity(queue_cap),
                            },
                        ),
                    )
                    .map_err(|hecs::NoSuchEntity| AssumeControlError::NoSuchEntity { entity })?;
                Ok(EntityController { commander, entity })
            }
        }
    }
}

impl<T> InputController for EntityController<T>
where
    T: InputCommander + Send + 'static,
    T::Command: Send + Sync + 'static,
{
    fn control(&mut self, event: DeviceEvent, _res: &mut Res, world: &mut World) -> ControlResult {
        match world.query_one_mut::<&mut CommandQueue<T::Command>>(self.entity) {
            Ok(queue) => match self.commander.translate(event) {
                None => ControlResult::Ignored,
                Some(command) => {
                    if queue.commands.capacity() == queue.commands.len() {
                        queue.commands.pop_front();
                    }
                    queue.commands.push_back(command);
                    ControlResult::Consumed
                }
            },
            Err(_err) => ControlResult::ControlLost,
        }
    }
}

// /// A kind of [`InputController`]s that yield commands and sends them to a server and a command queue of an entity.
// pub struct ClientEntityController<T> {
//     commander: T,
//     entity: Entity,
//     netid: NetId,
// }

// impl<T> ClientEntityController<T>
// where
//     T: InputCommander,
//     T::Command: Send + Sync + 'static,
// {
//     pub fn assume_control(
//         commander: T,
//         queue_cap: usize,
//         entity: Entity,
//         world: &mut World,
//     ) -> Result<Self, AssumeControlError> {
//         match world.query_one_mut::<Controlled>(entity).is_ok() {
//             true => Err(AssumeControlError::AlreadyControlled { entity }),
//             false => {
//                 world.insert(
//                     entity,
//                     (
//                         CONTROLLED,
//                         CommandQueue::<T::Command> {
//                             commands: VecDeque::with_capacity(queue_cap),
//                         },
//                     ),
//                 );
//                 Ok(ClientEntityController { commander, entity })
//             }
//         }
//     }
// }

// impl<T> InputController for ClientEntityController<T>
// where
//     T: InputCommander + Send + 'static,
//     T::Command: Send + Sync + 'static,
// {
//     fn control(&mut self, event: DeviceEvent, _res: &mut Res, world: &mut World) -> ControlResult {
//         match world.query_one_mut::<&mut CommandQueue<T::Command>>(self.entity) {
//             Ok(queue) => match self.commander.translate(event) {
//                 None => ControlResult::Ignored,
//                 Some(command) => {
//                     if queue.commands.capacity() == queue.commands.len() {
//                         queue.commands.pop_front();
//                     }
//                     queue.commands.push_back(command);
//                     ControlResult::Consumed
//                 }
//             },
//             Err(err) => ControlResult::ControlLost,
//         }
//     }
// }
