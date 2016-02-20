extern crate time;

use time::{SteadyTime, Duration};
use std::collections::BTreeMap;
use std::cmp::Ordering;
use std::fmt;
use std::error::Error;
use std::any::Any;
use std::cmp::PartialEq;
use std::thread::sleep;

#[derive(Clone)]
struct Task<Token> where Token: Clone {
    interval: Duration,
    run_offset: Duration,
    token: Token,
    bond: TaskBond
}

#[derive(Clone, Debug)]
enum TaskBond {
    OneOff,
    Perpetual
}

impl<Token> Task<Token> where Token: Clone {
    fn new(interval: Duration, run_offset: Duration, bond: TaskBond, token: Token) -> Task<Token> {
        assert!(interval >= Duration::seconds(0)); // negative interval would make schedule go back in time!
        Task {
            interval: interval,
            run_offset: run_offset,
            bond: bond,
            token: token
        }
    }

    fn next(self) -> Task<Token> {
        Task {
            run_offset: self.run_offset + self.interval,
            .. self
        }
    }

    fn schedule(&self) -> Duration {
        self.run_offset + self.interval
    }
}

pub trait TimeSource {
    // Duration since this TimeSource was crated
    fn now(&self) -> Duration;
}

pub trait FastForward {
    fn fast_forward(&mut self, duration: Duration);
}

pub trait Wait {
    fn wait(&mut self, duration: Duration);
}

pub struct SteadyTimeSource {
    offset: SteadyTime
}

impl SteadyTimeSource {
    fn new() -> SteadyTimeSource {
        SteadyTimeSource {
            offset: SteadyTime::now()
        }
    }
}

impl Wait for SteadyTimeSource {
    fn wait(&mut self, duration: Duration) {
        sleep(std::time::Duration::new(
            duration.num_seconds() as u64,
            (duration.num_nanoseconds().expect("sleep duration too large") - duration.num_seconds() * 1_000_000_000) as u32
        ));
    }
}

impl TimeSource for SteadyTimeSource {
    fn now(&self) -> Duration {
        SteadyTime::now() - self.offset
    }
}

type TimePoint = u64;

enum SchedulerAction {
    None,
    Wait(Duration),
    Skip(Vec<TimePoint>),
    Yield(TimePoint)
}

pub enum Schedule<Token> {
    NextIn(Duration),
    Overrun(Vec<Token>),
    Current(Vec<Token>)
}

impl<Token> PartialEq for Schedule<Token> where Token: PartialEq<Token> {
    fn eq(&self, other: &Self) -> bool {
        match self {
            &Schedule::NextIn(ref duration) => if let &Schedule::NextIn(ref other_duration) = other {
                duration == other_duration
            } else {
                false
            },
            &Schedule::Overrun(ref tokens) => if let &Schedule::Overrun(ref other_tokens) = other {
                tokens == other_tokens
            } else {
                false
            },
            &Schedule::Current(ref tokens) => if let &Schedule::Current(ref other_tokens) = other {
                tokens == other_tokens
            } else {
                false
            }
        }
    }
}

impl<Token> fmt::Debug for Schedule<Token> where Token: fmt::Debug {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Schedule::NextIn(ref duration) => write!(f, "Schedule::NextIn({}ms)", duration.num_milliseconds()),
            &Schedule::Overrun(ref tokens) => write!(f, "Schedule::Overrun({:?})", tokens),
            &Schedule::Current(ref tokens) => write!(f, "Schedule::Current({:?})", tokens),
        }
    }
}

pub struct Scheduler<Token, TS> where TS: TimeSource, Token: Clone {
    time_point_interval: Duration,
    tasks: BTreeMap<TimePoint, Vec<Task<Token>>>,
    time_source: TS
}

impl<Token> Scheduler<Token, SteadyTimeSource> where Token: Clone {
    pub fn new(time_point_interval: Duration) -> Scheduler<Token, SteadyTimeSource> {
        Scheduler::with_time_source(time_point_interval, SteadyTimeSource::new())
    }
}

impl<Token, TS> Scheduler<Token, TS> where TS: TimeSource, Token: Clone {
    pub fn with_time_source(time_point_interval: Duration, time_source: TS) -> Scheduler<Token, TS> {
        assert!(time_point_interval > Duration::seconds(0));
        Scheduler {
            time_point_interval: time_point_interval,
            tasks: BTreeMap::new(),
            time_source: time_source
        }
    }

    fn schedule(&mut self, task: Task<Token>) {
        let time_point = self.to_time_point(task.schedule());
        self.tasks.entry(time_point).or_insert(Vec::new()).push(task);
    }

    pub fn after(&mut self, duration: Duration, token: Token) {
        let task = Task::new(duration, self.time_source.now(), TaskBond::OneOff, token);
        self.schedule(task);
    }

    pub fn every(&mut self, duration: Duration, token: Token) {
        let task = Task::new(duration, self.time_source.now(), TaskBond::Perpetual, token);
        self.schedule(task);
    }

    fn next_action(&self) -> SchedulerAction {
        let now = self.time_source.now();
        let current_time_point = self.to_time_point(now);

        match self.tasks.iter().next() {
            None => SchedulerAction::None,
            Some((&time_point, _)) => {
                match time_point.cmp(&current_time_point) {
                    Ordering::Greater => SchedulerAction::Wait((self.to_duration(time_point)) - now),
                    Ordering::Less => SchedulerAction::Skip(self.tasks.iter().take_while(|&(&time_point, &_)| time_point < current_time_point).map(|(time_point, _)| time_point.clone()).collect()),
                    Ordering::Equal => SchedulerAction::Yield(time_point)
                }
            }
        }
    }

    pub fn next(&mut self) -> Option<Schedule<Token>> {
        match self.next_action() {
            SchedulerAction::None => None,
            SchedulerAction::Wait(duration) => {
                Some(Schedule::NextIn(duration))
            },
            SchedulerAction::Skip(time_points) => {
                let mut overrun = Vec::new();

                overrun.extend(self.consume(time_points));
                // collect all reschedules of consumed tasks if they end up overrun already
                while let SchedulerAction::Skip(time_points) = self.next_action() {
                    overrun.extend(self.consume(time_points));
                }
                Some(Schedule::Overrun(overrun))
            },
            SchedulerAction::Yield(time_point) => {
                Some(Schedule::Current(self.consume(vec![time_point])))
            }
        }
    }

    pub fn cancel(&mut self, token: &Token) where Token: PartialEq<Token> {
        let mut empty_time_points = vec![];

        for (ref time_point, ref mut tasks) in self.tasks.iter_mut() {
            tasks.retain(|task| task.token != *token);
            if tasks.is_empty() {
                empty_time_points.push(*time_point.clone());
            }
        }

        for time_point in empty_time_points {
            self.tasks.remove(&time_point).unwrap();
        }
    }

    fn consume(&mut self, time_points: Vec<TimePoint>) -> Vec<Token> {
        let mut tasks: Vec<Task<Token>> = time_points.iter().flat_map(|time_point|
                self.tasks.remove(&time_point).unwrap()
            ).collect();

        tasks.sort_by(|a, b| a.run_offset.cmp(&b.run_offset));
        let tokens = tasks.iter().map(|ref task| task.token.clone()).collect();

        for task in tasks {
            match task.bond {
                TaskBond::Perpetual => self.schedule(task.next()),
                TaskBond::OneOff => ()
            };
        }
        tokens
    }

    fn to_time_point(&self, duration: Duration) -> TimePoint {
        // nanoseconds gives 15250 weeks or 299 years of duration max... should do?
        let interval = self.time_point_interval.num_nanoseconds().expect("interval too large");
        let duration = duration.num_nanoseconds().expect("duration too large");
        assert!(duration >= 0);

        (duration / interval) as TimePoint
    }

    fn to_duration(&self, time_point: TimePoint) -> Duration {
        Duration::nanoseconds(self.time_point_interval.num_nanoseconds().expect("time point interval too large") * time_point as i64)
    }
}

impl<Token, TS> FastForward for Scheduler<Token, TS> where TS: TimeSource + FastForward, Token: Clone {
    fn fast_forward(&mut self, duration: Duration) {
        self.time_source.fast_forward(duration);
    }
}

pub enum WaitError<Token> {
    Empty,
    Overrun(Vec<Token>)
}

impl<Token> PartialEq for WaitError<Token> where Token: PartialEq<Token> {
    fn eq(&self, other: &Self) -> bool {
        match self {
            &WaitError::Empty => if let &WaitError::Empty = other {
                true
            } else {
                false
            },
            &WaitError::Overrun(ref tokens) => if let &WaitError::Overrun(ref other_tokens) = other {
                tokens == other_tokens
            } else {
                false
            }
        }
    }
}

impl<Token> fmt::Debug for WaitError<Token> where Token: fmt::Debug {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &WaitError::Empty => write!(f, "WaitError::Empty"),
            &WaitError::Overrun(ref tokens) => write!(f, "WaitError::Overrun({:?})", tokens)
        }
    }
}

impl<Token> fmt::Display for WaitError<Token> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &WaitError::Empty => write!(f, "scheduler is empty"),
            &WaitError::Overrun(ref tokens) => write!(f, "scheduler overrun {} tokens", tokens.len())
        }
    }
}

impl<Token> Error for WaitError<Token> where Token: fmt::Debug + Any {
    fn description(&self) -> &str {
        "problem while waiting for next schedule"
    }
}

impl<Token, TS> Scheduler<Token, TS> where TS: TimeSource + Wait, Token: Clone {
    //TODO: some way to integrate it with other async events in the program in a way that:
    // * there is no need to loop over with short sleep - consume CPU
    // * there is no latency - at the moment token is abailable it gets consumed by clint no mater
    // if he waits for other events like channels or IO
    // next_try() - can do loop with sleep... may be part of bigger sollution
    // wait() - just blocking
    // wait_timeout() - if WaitTimeout trait is impl: could spawn thread and use park with timeout;
    // would give zero latency for scheduler but not for other async sources
    // select? - make it selectable; would need to be somehow generic so can be used with other things like IO,
    // channels etc. - only via FD and OS specific stuff (epoll) or with thread park (what select!
    // macro does in very complicated way) - impl is out of scope for this unless it can be a
    // client easily; also select! is unsable and will be so

    pub fn wait(&mut self) -> Result<Vec<Token>, WaitError<Token>> where TS: Wait {
        match self.next() {
            Option::Some(schedule) => match schedule {
                Schedule::NextIn(duration) => {
                    //TODO: can we protect against wait() that does not move us forward?
                    self.time_source.wait(duration);
                    self.wait()
                },
                Schedule::Overrun(overrun_tokens) => {
                    Err(WaitError::Overrun(overrun_tokens))
                },
                Schedule::Current(tokens) => {
                    Ok(tokens)
                }
            },
            Option::None => Err(WaitError::Empty)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use super::{Task, TaskBond};
    use time::Duration;

    struct MockTimeSource {
        current_time: Duration
    }

    impl MockTimeSource {
        fn new() -> MockTimeSource {
            MockTimeSource {
                current_time: Duration::seconds(0)
            }
        }
    }

    impl FastForward for MockTimeSource {
        fn fast_forward(&mut self, duration: Duration) {
            self.current_time = self.current_time + duration;
        }
    }

    impl Wait for MockTimeSource {
        fn wait(&mut self, duration: Duration) {
            self.current_time = self.current_time + duration;
        }
    }

    impl TimeSource for MockTimeSource {
        fn now(&self) -> Duration {
            self.current_time
        }
    }

    #[test]
    fn task_next_schedule() {
        let now = Duration::seconds(0);
        let interval = Duration::seconds(1);
        let task = Task::new(interval, now, TaskBond::OneOff, 42);

        assert_eq!(task.schedule(), now + interval);
        assert_eq!(task.next().next().schedule(), now + interval * 3);
    }

    #[test]
    fn scheduler_to_time_point() {
        let scheduler: Scheduler<(), _> = Scheduler::new(Duration::seconds(1));
        assert_eq!(scheduler.to_time_point(Duration::seconds(0)), 0);
        assert_eq!(scheduler.to_time_point(Duration::seconds(1)), 1);
        assert_eq!(scheduler.to_time_point(Duration::seconds(2)), 2);
        assert_eq!(scheduler.to_time_point(Duration::milliseconds(2000)), 2);
        assert_eq!(scheduler.to_time_point(Duration::milliseconds(100)), 0);
        assert_eq!(scheduler.to_time_point(Duration::milliseconds(1100)), 1);
        assert_eq!(scheduler.to_time_point(Duration::milliseconds(1500)), 1);
        assert_eq!(scheduler.to_time_point(Duration::milliseconds(1800)), 1);
        assert_eq!(scheduler.to_time_point(Duration::milliseconds(2800)), 2);
    }

    #[test]
    fn scheduler_to_time_point_limits() {
        let scheduler: Scheduler<(), _> = Scheduler::new(Duration::nanoseconds(1));
        assert_eq!(scheduler.to_time_point(Duration::seconds(0)), 0);

        let scheduler: Scheduler<(), _> = Scheduler::new(Duration::weeks(15250) / 2);
        assert_eq!(scheduler.to_time_point(Duration::weeks(0)), 0);
        assert_eq!(scheduler.to_time_point(Duration::weeks(15250) / 2), 1);
        assert_eq!(scheduler.to_time_point(Duration::weeks(15250)), 2);
    }

    #[test]
    fn scheduler_to_duration() {
        let scheduler: Scheduler<(), _> = Scheduler::new(Duration::seconds(1));
        assert_eq!(scheduler.to_duration(0), Duration::seconds(0));
        assert_eq!(scheduler.to_duration(1), Duration::seconds(1));
        assert_eq!(scheduler.to_duration(2), Duration::seconds(2));
    }

    #[test]
    fn scheduler_to_duration_limits() {
        let scheduler: Scheduler<(), _> = Scheduler::new(Duration::nanoseconds(1));
        assert_eq!(scheduler.to_duration(0), Duration::nanoseconds(0));
        assert_eq!(scheduler.to_duration(1), Duration::nanoseconds(1));

        let scheduler: Scheduler<(), _> = Scheduler::new(Duration::weeks(15250) / 2);
        assert_eq!(scheduler.to_duration(0), Duration::seconds(0));
        assert_eq!(scheduler.to_duration(1), Duration::weeks(15250) / 2);
        assert_eq!(scheduler.to_duration(2), Duration::weeks(15250));
    }

    #[test]
    fn scheduler_empty() {
        let mut scheduler: Scheduler<(), _> = Scheduler::new(Duration::seconds(1));
        assert_eq!(scheduler.next(), Option::None);
        assert_eq!(scheduler.next(), Option::None);
    }

    #[test]
    fn scheduler_after() {
        let mut scheduler = Scheduler::with_time_source(Duration::seconds(1), MockTimeSource::new());

        scheduler.after(Duration::seconds(0), 0);
        assert_eq!(scheduler.next(), Option::Some(Schedule::Current(vec![0])));

        scheduler.after(Duration::seconds(1), 1);
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::seconds(1))));
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::seconds(1))));

        scheduler.fast_forward(Duration::milliseconds(100));
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::milliseconds(900))));
        scheduler.fast_forward(Duration::milliseconds(900));
        assert_eq!(scheduler.next(), Option::Some(Schedule::Current(vec![1])));
        assert_eq!(scheduler.next(), Option::None);
    }

    #[test]
    fn scheduler_every() {
        let mut scheduler = Scheduler::with_time_source(Duration::seconds(1), MockTimeSource::new());

        scheduler.every(Duration::seconds(1), 1);
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::seconds(1))));
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::seconds(1))));

        scheduler.fast_forward(Duration::milliseconds(100));
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::milliseconds(900))));
        scheduler.fast_forward(Duration::milliseconds(900));
        assert_eq!(scheduler.next(), Option::Some(Schedule::Current(vec![1])));
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::seconds(1))));

        scheduler.fast_forward(Duration::milliseconds(600));
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::milliseconds(400))));
        scheduler.fast_forward(Duration::milliseconds(500));
        assert_eq!(scheduler.next(), Option::Some(Schedule::Current(vec![1])));
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::milliseconds(900))));
    }

    #[test]
    fn scheduler_every_with_overrun() {
        let mut scheduler = Scheduler::with_time_source(Duration::seconds(1), MockTimeSource::new());

        scheduler.every(Duration::seconds(1), 1);
        scheduler.fast_forward(Duration::seconds(4));
        assert_eq!(scheduler.next(), Option::Some(Schedule::Overrun(vec![1, 1, 1])));
        assert_eq!(scheduler.next(), Option::Some(Schedule::Current(vec![1])));
    }

    #[test]
    fn scheduler_limits() {
        let mut scheduler = Scheduler::with_time_source(Duration::nanoseconds(1), MockTimeSource::new());

        scheduler.after(Duration::nanoseconds(1), 1);
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::nanoseconds(1))));

        scheduler.fast_forward(Duration::nanoseconds(1));
        assert_eq!(scheduler.next(), Option::Some(Schedule::Current(vec![1])));

        scheduler.after(Duration::weeks(15250), 2);
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::weeks(15250))));

        scheduler.fast_forward(Duration::weeks(15250));
        assert_eq!(scheduler.next(), Option::Some(Schedule::Current(vec![2])));

        let mut scheduler = Scheduler::with_time_source(Duration::weeks(15250) / 2, MockTimeSource::new());

        scheduler.after(Duration::weeks(15250) / 2, 1);
        assert_eq!(scheduler.next(), Option::Some(Schedule::NextIn(Duration::weeks(15250) / 2)));

        scheduler.fast_forward(Duration::weeks(15250) / 2);
        assert_eq!(scheduler.next(), Option::Some(Schedule::Current(vec![1])));
    }

    #[test]
    fn scheduler_wait() {
        let mut scheduler = Scheduler::with_time_source(Duration::seconds(1), MockTimeSource::new());

        scheduler.after(Duration::seconds(0), 0);
        scheduler.after(Duration::seconds(1), 1);
        scheduler.after(Duration::seconds(2), 2);

        assert_eq!(scheduler.wait(), Result::Ok(vec![0]));
        assert_eq!(scheduler.wait(), Result::Ok(vec![1]));
        assert_eq!(scheduler.wait(), Result::Ok(vec![2]));
    }

    #[test]
    fn scheduler_wait_with_overrun() {
        let mut scheduler = Scheduler::with_time_source(Duration::seconds(1), MockTimeSource::new());

        scheduler.after(Duration::seconds(0), 0);
        scheduler.after(Duration::seconds(1), 1);
        scheduler.after(Duration::seconds(2), 2);

        scheduler.fast_forward(Duration::seconds(2));
        assert_eq!(scheduler.wait(), Result::Err(WaitError::Overrun(vec![0, 1])));
        assert_eq!(scheduler.wait(), Result::Ok(vec![2]));
    }

    #[test]
    fn scheduler_wait_real_time() {
        let mut scheduler = Scheduler::new(Duration::milliseconds(100));

        scheduler.after(Duration::milliseconds(0), 0);
        scheduler.after(Duration::milliseconds(100), 1);
        scheduler.after(Duration::milliseconds(200), 2);

        assert_eq!(scheduler.wait(), Result::Ok(vec![0]));
        assert_eq!(scheduler.wait(), Result::Ok(vec![1]));
        assert_eq!(scheduler.wait(), Result::Ok(vec![2]));
    }

    #[test]
    fn scheduler_cancel_whole_time_point() {
        let mut scheduler = Scheduler::new(Duration::milliseconds(100));

        scheduler.after(Duration::milliseconds(0), 0);
        scheduler.after(Duration::milliseconds(100), 1);
        scheduler.after(Duration::milliseconds(200), 2);
        scheduler.after(Duration::milliseconds(300), 4);

        scheduler.cancel(&1);
        scheduler.cancel(&2);

        assert_eq!(scheduler.wait(), Result::Ok(vec![0]));
        assert_eq!(scheduler.wait(), Result::Ok(vec![4]));
    }

    #[test]
    fn scheduler_cancel_single_token() {
        let mut scheduler = Scheduler::new(Duration::milliseconds(100));

        scheduler.after(Duration::milliseconds(0), 0);
        scheduler.after(Duration::milliseconds(100), 1);
        scheduler.after(Duration::milliseconds(100), 2);
        scheduler.after(Duration::milliseconds(100), 3);
        scheduler.after(Duration::milliseconds(200), 4);
        scheduler.after(Duration::milliseconds(200), 5);

        scheduler.cancel(&1);
        scheduler.cancel(&4);

        assert_eq!(scheduler.wait(), Result::Ok(vec![0]));
        assert_eq!(scheduler.wait(), Result::Ok(vec![2, 3]));
        assert_eq!(scheduler.wait(), Result::Ok(vec![5]));
    }
}

