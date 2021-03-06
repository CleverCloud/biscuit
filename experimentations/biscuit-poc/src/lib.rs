extern crate datalog_with_constraints as datalog;
extern crate biscuit_vrf as vrf;
extern crate rand;
extern crate curve25519_dalek;
extern crate serde;
extern crate serde_cbor;
extern crate sha2;
extern crate hmac;
#[macro_use]
extern crate nom;

use curve25519_dalek::ristretto::RistrettoPoint;
use serde::{Serialize, Deserialize};
use vrf::KeyPair;
use datalog::{World, Fact, Rule, ID, SymbolTable};
use std::collections::HashSet;
use ser::SerializedBiscuit;
use builder::BlockBuilder;
use verifier::Verifier;

mod ser;
mod builder;
mod verifier;
mod sealed;

pub fn default_symbol_table() -> SymbolTable {
  let mut syms = SymbolTable::new();
  syms.insert("authority");
  syms.insert("ambient");
  syms.insert("resource");
  syms.insert("operation");
  syms.insert("right");
  syms.insert("current_time");
  syms.insert("revocation_id");

  syms
}

#[derive(Clone,Debug)]
pub struct Biscuit {
  authority: Block,
  blocks: Vec<Block>,
  symbols: SymbolTable,
  container: Option<SerializedBiscuit>,
}

impl Biscuit {
  pub fn new(root: &KeyPair, authority: &Block) -> Result<Biscuit, String> {
    let authority = authority.clone();

    let mut symbols = default_symbol_table();
    let h1 = symbols.symbols.iter().collect::<HashSet<_>>();
    let h2 = authority.symbols.symbols.iter().collect::<HashSet<_>>();

    if !h1.is_disjoint(&h2) {
      return Err(String::from("symbol tables should have no overlap"));
    }

    if authority.index as usize != 0 {
      return Err(String::from("invalid block index"));
    }

    symbols.symbols.extend(authority.symbols.symbols.iter().cloned());

    let blocks = vec![];

    let container = Some(SerializedBiscuit::new(root, &authority));

    Ok(Biscuit { authority, blocks, symbols, container })
  }

  pub fn from(slice: &[u8], root: RistrettoPoint) -> Result<Self, String> {
    let container = SerializedBiscuit::from_slice(slice, root)?;

    let authority: Block = serde_cbor::from_slice(&container.authority).map_err(|e| format!("error deserializing authority block: {:?}", e))?;

    if authority.index != 0 {
      return Err(String::from("authority block should have index 0"));
    }

    let mut blocks = vec![];

    let mut index = 1;
    for block in container.blocks.iter() {
      let deser:Block = serde_cbor::from_slice(&block).map_err(|e| format!("error deserializing block: {:?}", e))?;
      if deser.index != index {
        return Err(format!("invalid index {} for block n°{}", deser.index, index));
      }
      blocks.push(deser);

      index += 1;
    }

    let mut symbols = default_symbol_table();
    symbols.symbols.extend(authority.symbols.symbols.iter().cloned());

    for block in blocks.iter() {
      symbols.symbols.extend(block.symbols.symbols.iter().cloned());
    }

    let container = Some(container);

    Ok(Biscuit { authority, blocks, symbols, container })
  }

  pub fn from_sealed(slice: &[u8], secret: &[u8]) -> Result<Self, String> {
    let container = sealed::SealedBiscuit::from_slice(slice, secret)?;

    let authority: Block = serde_cbor::from_slice(&container.authority).map_err(|e| format!("error deserializing authority block: {:?}", e))?;

    if authority.index != 0 {
      return Err(String::from("authority block should have index 0"));
    }

    let mut blocks = vec![];

    let mut index = 1;
    for block in container.blocks.iter() {
      let deser:Block = serde_cbor::from_slice(&block).map_err(|e| format!("error deserializing block: {:?}", e))?;
      if deser.index != index {
        return Err(format!("invalid index {} for block n°{}", deser.index, index));
      }
      blocks.push(deser);

      index += 1;
    }

    let mut symbols = default_symbol_table();
    symbols.symbols.extend(authority.symbols.symbols.iter().cloned());

    for block in blocks.iter() {
      symbols.symbols.extend(block.symbols.symbols.iter().cloned());
    }

    let container = None;

    Ok(Biscuit { authority, blocks, symbols, container })
  }

  pub fn to_vec(&self) -> Option<Vec<u8>> {
    self.container.as_ref().map(|c| c.to_vec())
  }

  pub fn seal(&self, secret: &[u8]) -> Vec<u8> {
    let sealed = sealed::SealedBiscuit::from_token(self, secret);
    sealed.to_vec()
  }

  pub fn check(&self, mut ambient_facts: Vec<Fact>, ambient_rules: Vec<Rule>, ambient_caveats: Vec<Rule>)
    -> Result<(), Vec<String>> {

    let mut world = World::new();

    let authority_index = self.symbols.get("authority").unwrap();
    let ambient_index = self.symbols.get("ambient").unwrap();

    for fact in self.authority.facts.iter().cloned() {
      if fact.0.ids[0] != ID::Symbol(authority_index) {
        return Err(vec![format!("invalid authority fact: {}", self.symbols.print_fact(&fact))]);
      }

      world.facts.insert(fact);
    }

    // autority caveats are actually rules
    for rule in self.authority.caveats.iter().cloned() {
      world.rules.push(rule);
    }

    world.run();

    if world.facts.iter().find(|fact| fact.0.ids[0] != ID::Symbol(authority_index)).is_some() {
      return Err(vec![String::from("generated authority facts should have the authority context")]);
    }

    //remove authority rules: we cannot create facts anymore in authority scope
    //w.rules.clear();

    for fact in ambient_facts.drain(..) {
      if fact.0.ids[0] != ID::Symbol(ambient_index) {
        return Err(vec![format!("invalid ambient fact: {}", self.symbols.print_fact(&fact))]);
      }

      world.facts.insert(fact);
    }

    for rule in ambient_rules.iter().cloned() {
      world.rules.push(rule);
    }

    world.run();

    // we only keep the verifier rules
    world.rules = ambient_rules;

    let mut errors = vec![];
    for (i, block) in self.blocks.iter().enumerate() {
      let w = world.clone();

      match block.check(i, w, &self.symbols, &ambient_caveats) {
        Err(mut e) => {
          errors.extend(e.drain(..));
        },
        Ok(_) => {}
      }
    }

    if errors.is_empty() {
      Ok(())
    } else {
      Err(errors)
    }
  }

  pub fn create_block(&self) -> BlockBuilder {
    BlockBuilder::new((1 + self.blocks.len()) as u32, self.symbols.clone())
  }

  pub fn append(&self, keypair: &KeyPair, block: Block) -> Result<Self, String> {
    if self.container.is_none() {
      return Err(String::from("cannot append a bock to a sealed token"));
    }

    let h1 = self.symbols.symbols.iter().collect::<HashSet<_>>();
    let h2 = block.symbols.symbols.iter().collect::<HashSet<_>>();

    if !h1.is_disjoint(&h2) {
      return Err(String::from("symbol tables should have no overlap"));
    }

    if block.index as usize != 1 + self.blocks.len() {
      return Err(String::from("invalid block index"));
    }

    let authority = self.authority.clone();
    let mut blocks = self.blocks.clone();
    let mut symbols = self.symbols.clone();
    let container = self.container.as_ref().map(|c| c.append(keypair, &block));
    symbols.symbols.extend(block.symbols.symbols.iter().cloned());
    blocks.push(block);

    Ok(Biscuit { authority, blocks, symbols, container })
  }

  pub fn adjust_authority_symbols(block: &mut Block) {
    let base_symbols = default_symbol_table();

    let new_syms = block.symbols.symbols.split_off(base_symbols.symbols.len());

    block.symbols.symbols = new_syms;
  }

  pub fn adjust_block_symbols(&self, block: &mut Block) {
    let new_syms = block.symbols.symbols.split_off(self.symbols.symbols.len());

    block.symbols.symbols = new_syms;
  }

  pub fn print(&self) -> String {
    let authority = print_block(&self.symbols, &self.authority);
    let blocks: Vec<_> = self.blocks.iter().map(|b| print_block(&self.symbols, b)).collect();

    format!("Biscuit {{\n\tsymbols: {:?}\n\tauthority:\n{}\n\tblocks: [\n\t\t{}]\n}}",
      self.symbols.symbols, authority, blocks.join(",\n\t"))
  }
}

fn print_block(symbols: &SymbolTable, block: &Block) -> String {
  let facts: Vec<_> = block.facts.iter().map(|f| symbols.print_fact(f)).collect();
  let rules: Vec<_> = block.caveats.iter().map(|r| symbols.print_rule(r)).collect();

  format!("Block[{}] {{\n\t\tsymbols: {:?}\n\t\tfacts: [\n\t\t\t{}]\n\t\trules:[\n\t\t\t{}]\n}}",
    block.index, block.symbols.symbols, facts.join(",\n\t\t\t"), rules.join(",\n\t\t\t"))

}

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct Block {
  pub index: u32,
  pub symbols: SymbolTable,
  pub facts: Vec<Fact>,
  pub caveats: Vec<Rule>,
}

impl Block {
  pub fn new(index: u32, base_symbols: SymbolTable) -> Block {
    Block {
      index,
      symbols: base_symbols,
      facts: vec![],
      caveats: vec![],
    }
  }

  pub fn symbol_add(&mut self, s: &str) -> ID {
    self.symbols.add(s)
  }

  pub fn symbol_insert(&mut self, s: &str) -> u64 {
    self.symbols.insert(s)
  }

  pub fn check(&self, i: usize, mut world: World, symbols: &SymbolTable, verifier_caveats: &[Rule])
    -> Result<(), Vec<String>> {
    let authority_index = symbols.get("authority").unwrap();
    let ambient_index = symbols.get("ambient").unwrap();

    for fact in self.facts.iter().cloned() {
      if fact.0.ids[0] == ID::Symbol(authority_index) ||
        fact.0.ids[0] == ID::Symbol(ambient_index) {
        return Err(vec![format!("Block {}: invalid fact: {}", i, symbols.print_fact(&fact))]);
      }

      world.facts.insert(fact);
    }

    world.run();

    let mut errors = vec![];
    for (j, caveat) in self.caveats.iter().enumerate() {
      let res = world.query_rule(caveat.clone());
      if res.is_empty() {
        errors.push(format!("Block {}: caveat {} failed: {}", i, j, symbols.print_rule(caveat)));
      }
    }

    for (i, caveat) in verifier_caveats.iter().enumerate() {
      let res = world.query_rule(caveat.clone());
      if res.is_empty() {
        errors.push(format!("Verifier caveat {} failed: {}", i, symbols.print_rule(caveat)));
      }
    }


    if errors.is_empty() {
      Ok(())
    } else {
      Err(errors)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use rand::prelude::*;
  use crate::ser::SerializedBiscuit;
  use crate::builder::{BlockBuilder,fact,rule,pred,int,string,date,var,s};
  use crate::verifier::Verifier;
  use nom::HexDisplay;
  use vrf::KeyPair;
  use std::time::{Duration, SystemTime};

  #[test]
  fn basic() {
    let mut rng: StdRng = SeedableRng::seed_from_u64(0);
    let root = KeyPair::new(&mut rng);

    let serialized1 = {
      let symbols = default_symbol_table();
      let mut authority_block = BlockBuilder::new(0, symbols);

      authority_block.add_fact(&fact("right", &[s("authority"), s("file1"), s("read")]));
      authority_block.add_fact(&fact("right", &[s("authority"), s("file2"), s("read")]));
      authority_block.add_fact(&fact("right", &[s("authority"), s("file1"), s("write")]));

      let biscuit1 = Biscuit::new(&root, &authority_block.to_block()).unwrap();

      println!("biscuit1 (authority): {}", biscuit1.print());

      biscuit1.to_vec().unwrap()
    };

    println!("generated biscuit token: {} bytes:\n{}", serialized1.len(), serialized1.to_hex(16));
    println!("generated biscuit token: {} bytes", serialized1.len());
    //println!("parsed: {:#?}", serde_cbor::from_slice::<serde_cbor::Value>(&serialized1));
    //panic!();

    let serialized2 = {
      let biscuit1_deser = Biscuit::from(&serialized1, root.public).unwrap();

      // new caveat: can only have read access1
      let mut block2 = biscuit1_deser.create_block();

      block2.add_caveat(&rule("caveat1", &[var(0)], &[
        pred("resource", &[s("ambient"), var(0)]),
        pred("operation", &[s("ambient"), s("read")]),
        pred("right", &[s("authority"), var(0), s("read")])
      ]));

      let keypair2 = KeyPair::new(&mut rng);
      let biscuit2 = biscuit1_deser.append(&keypair2, block2.to_block()).unwrap();

      println!("biscuit2 (1 caveat): {}", biscuit2.print());

      biscuit2.to_vec().unwrap()
    };

    println!("generated biscuit token 2: {} bytes\n{}", serialized2.len(), serialized2.to_hex(16));
    println!("generated biscuit token 2: {} bytes", serialized2.len());

    let serialized3 = {
      let biscuit2_deser = Biscuit::from(&serialized2, root.public).unwrap();

      // new caveat: can only access file1
      let mut block3 = biscuit2_deser.create_block();

      block3.add_caveat(&rule("caveat2", &[s("file1")], &[
        pred("resource", &[s("ambient"), s("file1")])
      ]));

      let keypair3 = KeyPair::new(&mut rng);
      let biscuit3 = biscuit2_deser.append(&keypair3, block3.clone().to_block()).unwrap();

      biscuit3.to_vec().unwrap()
    };

    println!("generated biscuit token 3: {} bytes\n{}", serialized3.len(), serialized3.to_hex(16));
    println!("generated biscuit token 3: {} bytes", serialized3.len());
    //panic!();

    let final_token = Biscuit::from(&serialized3, root.public).unwrap();
    println!("final token:\n{}", final_token.print());
    {
      let mut symbols = final_token.symbols.clone();

      let facts = vec![
        fact("resource", &[s("ambient"), s("file1")]),
        fact("operation", &[s("ambient"), s("read")]),
      ];
      let mut ambient_facts = vec![];

      for fact in facts.iter() {
        ambient_facts.push(fact.convert(&mut symbols));
      }

      //println!("final token: {:#?}", final_token);
      //println!("ambient facts: {:#?}", ambient_facts);
      let res = final_token.check(ambient_facts, vec![], vec![]);
      println!("res1: {:?}", res);
      res.unwrap();
    }

    {
      let mut symbols = final_token.symbols.clone();

      let facts = vec![
        fact("resource", &[s("ambient"), s("file2")]),
        fact("operation", &[s("ambient"), s("write")]),
      ];
      let mut ambient_facts = vec![];

      for fact in facts.iter() {
        ambient_facts.push(fact.convert(&mut symbols));
      }

      let res = final_token.check(ambient_facts, vec![], vec![]);
      println!("res2: {:#?}", res);
      assert_eq!(res,
        Err(vec![
          "Block 0: caveat 0 failed: caveat1(0?) <- resource(#ambient, 0?) && operation(#ambient, #read) && right(#authority, 0?, #read) | ".to_string(),
          "Block 1: caveat 0 failed: caveat2(#file1) <- resource(#ambient, #file1) | ".to_string()
        ]));
    }
  }

  #[test]
  fn folders() {
    let mut rng: StdRng = SeedableRng::seed_from_u64(0);
    let root = KeyPair::new(&mut rng);

    let symbols = default_symbol_table();
    let mut authority_block = BlockBuilder::new(0, symbols);

    authority_block.add_right("/folder1/file1", "read");
    authority_block.add_right("/folder1/file1", "write");
    authority_block.add_right("/folder1/file2", "read");
    authority_block.add_right("/folder1/file2", "write");
    authority_block.add_right("/folder2/file3", "read");

    let biscuit1 = Biscuit::new(&root, &authority_block.to_block()).unwrap();

    println!("biscuit1 (authority): {}", biscuit1.print());

    let mut block2 = biscuit1.create_block();

    block2.resource_prefix("/folder1/");
    block2.check_right("read");

    let keypair2 = KeyPair::new(&mut rng);
    let biscuit2 = biscuit1.append(&keypair2, block2.to_block()).unwrap();

    {
      let mut verifier = Verifier::new();
      verifier.resource("/folder1/file1");
      verifier.operation("read");

      let res = verifier.verify(biscuit2.clone());
      println!("res1: {:?}", res);
      res.unwrap();
    }

    {
      let mut verifier = Verifier::new();
      verifier.resource("/folder2/file3");
      verifier.operation("read");

      let res = verifier.verify(biscuit2.clone());
      println!("res2: {:?}", res);
      assert_eq!(res,
        Err(vec![
          "Block 0: caveat 0 failed: prefix(0?) <- resource(#ambient, 0?) | 0? matches /folder1/*".to_string(),
        ]));
    }

    {
      let mut verifier = Verifier::new();
      verifier.resource("/folder2/file1");
      verifier.operation("write");

      let res = verifier.verify(biscuit2.clone());
      println!("res3: {:?}", res);
      assert_eq!(res,
        Err(vec![
          "Block 0: caveat 0 failed: prefix(0?) <- resource(#ambient, 0?) | 0? matches /folder1/*".to_string(),
          "Block 0: caveat 1 failed: check_right(#read) <- resource(#ambient, 0?) && operation(#ambient, #read) && right(#authority, 0?, #read) | ".to_string()
        ]));
    }
  }

  #[test]
  fn constraints() {
    let mut rng: StdRng = SeedableRng::seed_from_u64(0);
    let root = KeyPair::new(&mut rng);

    let symbols = default_symbol_table();
    let mut authority_block = BlockBuilder::new(0, symbols);

    authority_block.add_right("file1", "read");
    authority_block.add_right("file2", "read");

    let biscuit1 = Biscuit::new(&root, &authority_block.to_block()).unwrap();

    println!("biscuit1 (authority): {}", biscuit1.print());

    let mut block2 = biscuit1.create_block();

    block2.expiration_date(SystemTime::now()+Duration::from_secs(30));
    block2.revocation_id(1234);

    let keypair2 = KeyPair::new(&mut rng);
    let biscuit2 = biscuit1.append(&keypair2, block2.to_block()).unwrap();

    {
      let mut verifier = Verifier::new();
      verifier.resource("file1");
      verifier.operation("read");
      verifier.time();

      let res = verifier.verify(biscuit2.clone());
      println!("res1: {:?}", res);
      res.unwrap();
    }

    {
      let mut verifier = Verifier::new();
      verifier.resource("file1");
      verifier.operation("read");
      verifier.time();
      verifier.revocation_check(&[0, 1, 2, 5, 1234]);

      let res = verifier.verify(biscuit2.clone());
      println!("res3: {:?}", res);

      // error message should be like this:
      //"Verifier caveat 0 failed: revocation_check(0?) <- revocation_id(0?) | 0? not in {2, 1234, 1, 5, 0}"
      assert!(res.is_err());
    }
  }

  #[test]
  fn sealed_token() {
    let mut rng: StdRng = SeedableRng::seed_from_u64(0);
    let root = KeyPair::new(&mut rng);

    let symbols = default_symbol_table();
    let mut authority_block = BlockBuilder::new(0, symbols);

    authority_block.add_right("/folder1/file1", "read");
    authority_block.add_right("/folder1/file1", "write");
    authority_block.add_right("/folder1/file2", "read");
    authority_block.add_right("/folder1/file2", "write");
    authority_block.add_right("/folder2/file3", "read");

    let biscuit1 = Biscuit::new(&root, &authority_block.to_block()).unwrap();

    println!("biscuit1 (authority): {}", biscuit1.print());

    let mut block2 = biscuit1.create_block();

    block2.resource_prefix("/folder1/");
    block2.check_right("read");

    let keypair2 = KeyPair::new(&mut rng);
    let biscuit2 = biscuit1.append(&keypair2, block2.to_block()).unwrap();

    println!("biscuit2:\n{:#?}", biscuit2);
    panic!();

    {
      let mut verifier = Verifier::new();
      verifier.resource("/folder1/file1");
      verifier.operation("read");

      let res = verifier.verify(biscuit2.clone());
      println!("res1: {:?}", res);
      res.unwrap();
    }

    let serialized = biscuit2.to_vec().unwrap();
    println!("biscuit2 serialized ({} bytes):\n{}", serialized.len(), serialized.to_hex(16));

    let secret = b"secret key";
    let sealed = biscuit2.seal(&secret[..]);
    println!("biscuit2 sealed ({} bytes):\n{}", sealed.len(), sealed.to_hex(16));

    let biscuit3 = Biscuit::from_sealed(&sealed, &secret[..]).unwrap();

    {
      let mut verifier = Verifier::new();
      verifier.resource("/folder1/file1");
      verifier.operation("read");

      let res = verifier.verify(biscuit3.clone());
      println!("res1: {:?}", res);
      res.unwrap();
    }
  }
}
