//! The names an agent gets when nobody names it: first names of influential computer
//! scientists, mathematicians and physicists. At least half of them are women.

use rand::seq::IteratorRandom;
use wf_core::name::AgentName;

/// Ada Lovelace, Grace Hopper, Frances Allen, Jean Bartik, Betty Holberton, Kathleen Booth,
/// Klára Dán von Neumann, Annie Easley, Karen Spärck Jones, Evelyn Berezin, Joan Clarke,
/// Hedy Lamarr, Dorothy Vaughan, Katherine Johnson, Emmy Noether, Sophie Germain,
/// Sofia Kovalevskaya, Hypatia, Maryam Mirzakhani, Julia Robinson, Olga Ladyzhenskaya,
/// Mary Somerville, Florence Nightingale, Edith Clarke, Marie Curie, Irène Joliot-Curie,
/// Lise Meitner, Maria Goeppert Mayer, Émilie du Châtelet, Rosalind Franklin, Vera Rubin,
/// Henrietta Leavitt, Cecilia Payne-Gaposchkin, Caroline Herschel, Mildred Dresselhaus.
const WOMEN: &[&str] = &[
    "ada",
    "grace",
    "frances",
    "jean",
    "betty",
    "kathleen",
    "klara",
    "annie",
    "karen",
    "evelyn",
    "joan",
    "hedy",
    "dorothy",
    "katherine",
    "emmy",
    "sophie",
    "sofia",
    "hypatia",
    "maryam",
    "julia",
    "olga",
    "mary",
    "florence",
    "edith",
    "marie",
    "irene",
    "lise",
    "maria",
    "emilie",
    "rosalind",
    "vera",
    "henrietta",
    "cecilia",
    "caroline",
    "mildred",
];

/// Alan Turing, Edsger Dijkstra, Alonzo Church, Haskell Curry, John von Neumann,
/// Dennis Ritchie, Niklaus Wirth, Robin Milner, Charles Babbage, Kurt Gödel, Leonhard Euler,
/// Carl Friedrich Gauss, Bernhard Riemann, David Hilbert, Georg Cantor, Henri Poincaré,
/// Évariste Galois, Srinivasa Ramanujan, Gottfried Leibniz, Blaise Pascal, Niels Bohr,
/// Richard Feynman, Paul Dirac, Enrico Fermi, Erwin Schrödinger, Max Planck, Isaac Newton,
/// James Clerk Maxwell, Michael Faraday, Satyendra Nath Bose, Abdus Salam, Hideki Yukawa.
const MEN: &[&str] = &[
    "alan",
    "edsger",
    "alonzo",
    "haskell",
    "john",
    "dennis",
    "niklaus",
    "robin",
    "charles",
    "kurt",
    "leonhard",
    "carl",
    "bernhard",
    "david",
    "georg",
    "henri",
    "evariste",
    "srinivasa",
    "gottfried",
    "blaise",
    "niels",
    "richard",
    "paul",
    "enrico",
    "erwin",
    "max",
    "isaac",
    "james",
    "michael",
    "satyendra",
    "abdus",
    "hideki",
];

/// A random name that is not `taken`. Once every name is, a name with a number: `ada-2`, then
/// `ada-3` once those are taken too.
pub fn free(taken: &[AgentName]) -> AgentName {
    free_with(taken, &mut rand::rng())
}

fn free_with(taken: &[AgentName], rng: &mut impl rand::Rng) -> AgentName {
    let is_free = |name: &AgentName| !taken.contains(name);
    let plain = WOMEN.iter().chain(MEN).map(|base| name(base.to_string()));
    if let Some(plain) = plain.filter(is_free).choose(rng) {
        return plain;
    }
    (2..)
        .find_map(|number| {
            WOMEN
                .iter()
                .chain(MEN)
                .map(|base| name(format!("{base}-{number}")))
                .filter(is_free)
                .choose(rng)
        })
        .expect("there are fewer taken names than numbers")
}

fn name(name: String) -> AgentName {
    AgentName::new(name).expect("every name on the list is a valid agent name")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rand::SeedableRng;

    use super::*;

    fn all() -> Vec<&'static str> {
        WOMEN.iter().chain(MEN).copied().collect()
    }

    #[test]
    fn at_least_half_of_the_names_are_women() {
        assert!(
            WOMEN.len() >= MEN.len(),
            "{} women, {} men",
            WOMEN.len(),
            MEN.len()
        );
    }

    #[test]
    fn every_name_is_a_valid_agent_name_and_unique() {
        for name in all() {
            assert!(AgentName::new(name).is_ok(), "{name}");
        }
        let unique: BTreeSet<_> = all().into_iter().collect();
        assert_eq!(unique.len(), all().len());
    }

    #[test]
    fn a_free_name_is_not_taken() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let taken: Vec<AgentName> = all()[1..]
            .iter()
            .map(|name| name.to_string())
            .map(name)
            .collect();
        assert_eq!(free_with(&taken, &mut rng).as_str(), all()[0]);
    }

    #[test]
    fn once_every_name_is_taken_a_number_is_added() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let mut taken: Vec<AgentName> = all()
            .iter()
            .map(|name| name.to_string())
            .map(name)
            .collect();
        let first = free_with(&taken, &mut rng);
        assert!(first.as_str().ends_with("-2"), "{first}");
        taken.extend(all().iter().map(|base| name(format!("{base}-2"))));
        let second = free_with(&taken, &mut rng);
        assert!(second.as_str().ends_with("-3"), "{second}");
    }
}
