use crate::rgd::*;
use crate::union_table::*;
use crate::union_to_ast::*;
use crate::util::*;
use crate::analyzer::*;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::{RwLock,Arc};
use crate::cpp_interface::*;
use protobuf::Message;
use crate::union_find::*;
use std::time;
use crate::parser::*;
use blockingqueue::BlockingQueue;
use crate::solution::Solution;
use crate::search_task::SearchTask;
use crate::op_def::*;
use z3::{Solver, Config, Context, ast, Model};
use z3::ast::Ast;
use crate::z3solver::*;
use fastgen_common::config;
use crate::fifo::PipeMsg;
use crate::fifo::GepMsg;
use std::io::BufReader;
use std::io::BufRead;
use std::{fs::File, io::{self, Read}};
use std::os::unix::io::{FromRawFd, RawFd};
use byteorder::{LittleEndian, ReadBytesExt};

fn bcount_filter(hitcount: u32, flipped: bool, msg_type: u16, localcnt: u32 ) -> bool {
  hitcount <= 5 && (!flipped) && msg_type != 3 && localcnt <= 16
}

fn qsym(pc: u64, direction:bool, msg_type: u16) -> bool {
  let qsym_result = unsafe { qsym_filter(pc, direction) };
  qsym_result && msg_type != 3
}

pub fn scan_nested_tasks(pipefd: RawFd,
    table: &UnionTable, tainted_size: usize, 
    branch_gencount: &Arc<RwLock<HashMap<(u64,u32,u32,u64), u32>>>,
    branch_fliplist: &Arc<RwLock<HashSet<(u64,u32,u32,u64)>>>,
    branch_hitcount: &Arc<RwLock<HashMap<(u64,u32,u32,u64), u32>>>, buf: &Vec<u8>,
    tb: &mut SearchTaskBuilder, solution_queue: BlockingQueue<Solution>) {


  let mut cfg = Config::new();
  cfg.set_timeout_msec(10000);
  let ctx = Context::new(&cfg);
  let solver = Solver::new(&ctx);


  unsafe { start_session(); }
  let t_start = time::Instant::now();
  let mut count = 0;
  let mut branch_local = HashMap::<(u64,u32),u32>::new();
  let f = unsafe { File::from_raw_fd(pipefd) };
  let mut reader = BufReader::new(f);
  loop {
    let rawmsg = PipeMsg::from_reader(&mut reader);
    if let Ok(msg) = rawmsg {
    let mut hitcount = 1;
    let mut gencount = 0;
    let mut flipped = false;
    let mut localcnt = 1;

    if msg.addr != 0 {
      if branch_local.contains_key(&(msg.addr,msg.ctx)) {
        localcnt = *branch_local.get(&(msg.addr,msg.ctx)).unwrap();
        localcnt += 1;
      }
    }
    branch_local.insert((msg.addr,msg.ctx), localcnt);

    debug!("tid: {} label: {} result: {} addr: {} ctx: {} localcnt: {} type: {}",
          msg.tid, msg.label, msg.result, msg.addr, msg.ctx, localcnt, msg.msgtype);

    if branch_hitcount.read().unwrap().contains_key(&(msg.addr,msg.ctx,localcnt, msg.result)) {
      hitcount = *branch_hitcount.read().unwrap().get(&(msg.addr,msg.ctx,localcnt,msg.result)).unwrap();
      hitcount += 1;
    }
    branch_hitcount.write().unwrap().insert((msg.addr,msg.ctx,localcnt,msg.result), hitcount);

    if branch_fliplist.read().unwrap().contains(&(msg.addr,msg.ctx,localcnt,msg.result)) {
      //info!("the branch is flipped");
      flipped = true;
    }

    if branch_gencount.read().unwrap().contains_key(&(msg.addr,msg.ctx,localcnt,msg.result)) {
      gencount = *branch_gencount.read().unwrap().get(&(msg.addr,msg.ctx,localcnt,msg.result)).unwrap();
    }

    let mut node_opt: Option<AstNode> = None;
    //let mut cons_reverse = Constraint::new();
    let mut inputs = HashSet::new();
    let mut node_cache = HashMap::new();
    if msg.msgtype == 1 {
      //node_opt = get_gep_constraint(label.1, label.2, table, &mut inputs, &mut node_cache);
      let rawmsg = GepMsg::from_reader(&mut reader);
      continue;
    } else if msg.msgtype == 0 {
      node_opt = get_one_constraint(msg.label, msg.result as u32, table, &mut inputs, &mut node_cache);
    } else if msg.msgtype == 2 {
      let mut data = Vec::new();
      if let Ok(memcmp_data_label) = reader.read_u32::<LittleEndian>() {
          if (memcmp_data_label != msg.label) {
            break;
          }
      } else {
          break;
      }
      for _i in 0..msg.result as usize {
        if let Ok(cur) = reader.read_u8() {
          data.push(cur);
          } else {
            break;
          }
      }
      if data.len() < msg.result as usize {
        break;
      }
      if localcnt > 64 { continue; }
      let (index, size) = get_fmemcmp_constraint(msg.label as u32, table, &mut inputs);
      if data.len() >= size {
        //unsafe { submit_fmemcmp(data.as_ptr(), index, size as u32, label.0, label.3); }
        let mut sol = HashMap::new(); 
        for i in 0..data.len() {  //minus 1
          sol.insert(index + i as u32, data[i]);
        }
        let rsol = Solution::new(sol, msg.tid, msg.addr, 0, 0, 0, index as usize, size);
        solution_queue.push(rsol);
      }
      continue;
    } else if msg.msgtype == 3 {
      node_opt = get_addcons_constraint(msg.label, msg.result as u32, table, &mut inputs, &mut node_cache);
    }


    if let Some(node) = node_opt { 
      //print_node(&node);

      debug!("direction is {}", msg.result);

      let breakdown = to_dnf(&node);
      let cons_breakdown = analyze_maps(&breakdown, &node_cache, buf);
      let reverse_cons_breakdown = de_morgan(&cons_breakdown);
      //cons_breakdown is a lor of lands
/*
      for row in &cons_breakdown {
        for item in row {
          print_node(&item.get_node());
        }
      }
*/
      let mut task;
      if msg.result == 1 {
        task = SearchTask::new((reverse_cons_breakdown,true), 
                              (cons_breakdown,false), 
                              msg.tid, msg.addr, msg.ctx, localcnt, msg.result);
      } else {
        task = SearchTask::new((cons_breakdown, false), 
                            (reverse_cons_breakdown, true), 
                            msg.tid, msg.addr, msg.ctx, localcnt, msg.result);
      }

      //tb.submit_task_rust(&task, solution_queue.clone(), true, &inputs);
      let is_flip = if config::QSYM_FILTER { qsym(msg.addr, msg.result == 1, msg.msgtype) } 
                else { bcount_filter(hitcount, flipped, msg.msgtype, localcnt) };
  
      
      //if hitcount <= 5 && (!flipped) && label.6 != 3 && localcnt <= 16 {
      if is_flip {
        count = count + 1;
        if !tb.submit_task_rust(&task, solution_queue.clone(), true, &inputs) {
          if msg.msgtype == 0 && config::HYBRID_SOLVER {
            if let Some(sol) =  solve_cond(msg.label, msg.result, table, &ctx, &solver) {
              let sol_size = sol.len();
              let z3_sol = Solution::new(sol, task.fid, task.addr, 
                  task.ctx, task.order, task.direction, 0, sol_size);
              solution_queue.push(z3_sol);
            }
          }
        }
      } else {
        tb.submit_task_rust(&task, solution_queue.clone(), false, &inputs);
      }

       
      let used_t1 = t_start.elapsed().as_secs() as u32;
      if (used_t1 > 90)  { //3min
        break;
      }
    }
    } else {
        break;
    }
  }
  info!("submitted {} tasks {:?}", count, t_start.elapsed());
}


#[cfg(test)]
mod tests {
  use super::*;
  use crate::cpp_interface::*;
  use protobuf::Message;
  use crate::fifo::*;
  use crate::util::*;
  use fastgen_common::config;
  use std::path::Path;
#[test]
  fn test_scan() {
    /*
       let id = unsafe {
       libc::shmget(
       0x1234,
       0xc00000000, 
       0o644 | libc::IPC_CREAT | libc::SHM_NORESERVE
       )
       };
       let ptr = unsafe { libc::shmat(id, std::ptr::null(), 0) as *mut UnionTable};
       let table = unsafe { & *ptr };

       unsafe { init_core(true,true); }
       let (labels,mut fmemcmpdata) = read_pipe(2);
       println!("labels len is {}", labels.len());
       let dedup = Arc::new(RwLock::new(HashSet::<(u64,u64,u32,u64)>::new()));
       let branch_hit = Arc::new(RwLock::new(HashMap::<(u64,u64,u32), u32>::new()));
    //let mut buf: Vec<u8> = Vec::with_capacity(15000);
    //buf.resize(15000, 0);
    let file_name = Path::new("/home/cju/fastgen/test/seed");
    let buf = read_from_file(&file_name);
    println!("before scanning\n");
    scan_nested_tasks(&labels, &mut fmemcmpdata, table, 15000, &dedup, &branch_hit, &buf);
    println!("after scanning\n");

    //    scan_tasks(&labels, &mut tasks, table);
    /*
    for task in tasks {
    println!("print task addr {} order {} ctx {}", task.get_addr(), task.get_order(), task.get_ctx());
    print_task(&task);
    let task_ser = task.write_to_bytes().unwrap();
    unsafe { submit_task(task_ser.as_ptr(), task_ser.len() as u32, true); }
    }
     */
    unsafe { aggregate_results(); }
    unsafe { fini_core(); }
    */
  }
}
