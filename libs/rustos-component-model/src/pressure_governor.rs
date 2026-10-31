//! Slow memory pressure response policies based on kernel-owned counters.
//! Never perform policy IPC or heap allocation inside page faults, allocator
//! charge/uncharge, scheduler dispatch or physical reclaim critical sections.
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Observation {
    pub job:u64,pub generation:u32,pub sequence:u64,pub now_ns:u64,
    pub used_pages:u64,pub high_pages:u64,pub min_pages:u64,
    pub max_pages:u64,pub high_events:u64,
}
impl Observation{
    pub fn valid(self)->bool{
        self.job!=0&&self.generation!=0&&self.sequence!=0
          &&self.now_ns!=0&&self.min_pages<=self.max_pages
          &&self.high_pages<=self.max_pages
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Decision {
    pub job:u64,pub generation:u32,pub sequence:u64,
    pub action:Action,pub target_pages:u64,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Action{Observe,TrimCaches,ApplyBackpressure,RequestReclaim,ExcludeFromOom}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error{Invalid,Capacity,Stale,Overflow,NoEvidence}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Entry{
    job:u64,generation:u32,last_sequence:u64,
    last_events:u64,last_action_ns:u64,credit:u64,
}
pub struct PressurePolicy<const N:usize> {
    entries:[Option<Entry>;N],
    cooldown_ns:u64,burst_limit:u64,
}
impl<const N:usize> PressurePolicy<N> {
    pub const fn new(cooldown_ns:u64,burst_limit:u64)->Self{
        Self{entries:[None;N],cooldown_ns,burst_limit}
    }
    pub fn assess(&mut self,o:Observation)->Result<Decision,Error>{
        if !o.valid(){return Err(Error::Invalid)}
        let existing=self.entries.iter().position(|e|e.is_some_and(|x|{
            x.job==o.job&&x.generation==o.generation
        }));
        let idx=match existing{
            Some(i)=>i,
            None=>self.entries.iter().position(Option::is_none)
                .ok_or(Error::Capacity)?,
        };
        let mut e=self.entries[idx].unwrap_or(Entry{
            job:o.job,generation:o.generation,
            last_sequence:0,last_events:0,last_action_ns:0,credit:0,
        });
        if o.sequence<=e.last_sequence||o.high_events<e.last_events {
            return Err(Error::Stale)
        }
        let elapsed=o.now_ns.saturating_sub(e.last_action_ns);
        let crossing=o.high_events>e.last_events;
        let excess=o.used_pages.saturating_sub(o.high_pages);
        let mut action=Action::Observe;
        let mut target=0u64;
        if o.used_pages<=o.min_pages {
            action=Action::ExcludeFromOom;
        } else if excess>0 {
            if e.last_action_ns==0||elapsed>=self.cooldown_ns {
                if crossing||excess>=o.max_pages/4 {
                    action=Action::RequestReclaim;
                    target=excess.min(self.burst_limit);
                } else {
                    action=Action::TrimCaches;
                    target=excess.min(self.burst_limit);
                }
                e.last_action_ns=o.now_ns;
            } else {
                action=Action::ApplyBackpressure;
            }
        }
        e.last_sequence=o.sequence;
        e.last_events=o.high_events;
        e.credit=e.credit.saturating_add(target);
        self.entries[idx]=Some(e);
        Ok(Decision{job:o.job,generation:o.generation,sequence:o.sequence,
            action,target_pages:target})
    }
    /// A reclaim request is not proof of reclaimed pages. Release credit only
    /// on independently observed resident-page decrease from ring0.
    pub fn acknowledge_reclaimed(&mut self,job:u64,generation:u32,
                                 observed_pages:u64)->Result<(),Error>{
        let e=self.entries.iter_mut().flatten().find(|x|{
            x.job==job&&x.generation==generation
        }).ok_or(Error::NoEvidence)?;
        if observed_pages>e.credit{return Err(Error::NoEvidence)}
        e.credit-=observed_pages;Ok(())
    }
    pub fn cancel_on_exit(&mut self,job:u64,generation:u32)->Result<(),Error>{
        let slot=self.entries.iter().position(|e|e.is_some_and(|x|{
            x.job==job&&x.generation==generation
        })).ok_or(Error::NoEvidence)?;
        self.entries[slot]=None;Ok(())
    }
    pub fn pending_pages(&self,job:u64,generation:u32)->Result<u64,Error>{
        self.entries.iter().flatten().find(|x|{
            x.job==job&&x.generation==generation
        }).map(|x|x.credit).ok_or(Error::NoEvidence)
    }
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn pressure_event_cannot_steal_oom_protection(){
        let mut p=PressurePolicy::<2>::new(1000,10);
        let observation=Observation{job:1,generation:2,sequence:1,now_ns:10,
            used_pages:100,high_pages:90,min_pages:20,max_pages:200,high_events:1};
        assert_eq!(p.assess(observation).unwrap().action,Action::RequestReclaim);
        let next=Observation{sequence:2,now_ns:11,used_pages:15,..observation};
        assert_eq!(p.assess(next).unwrap().action,Action::ExcludeFromOom);
    }
}
