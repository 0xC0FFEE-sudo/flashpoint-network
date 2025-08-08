use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyList};
use std::collections::HashSet;

#[derive(Clone, Debug)]
struct Intent {
    profit: f64,
    resources: Vec<String>,
    client_id: Option<String>,
    arrival_index: Option<usize>,
}

fn parse_intents(intents_py: &PyAny) -> PyResult<Vec<Intent>> {
    let list: &PyList = intents_py.downcast()?;
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        // Accept dicts with keys: profit, resources, client_id?, arrival_index?
        let d: &PyDict = item.downcast()?;

        let profit: f64 = match d.get_item("profit")? {
            Some(v) => v.extract()?,
            None => return Err(pyo3::exceptions::PyValueError::new_err("missing profit")),
        };

        let resources: Vec<String> = match d.get_item("resources")? {
            Some(v) => v.extract()?,
            None => return Err(pyo3::exceptions::PyValueError::new_err("missing resources")),
        };

        let client_id: Option<String> = match d.get_item("client_id")? {
            Some(v) => Some(v.extract()?),
            None => None,
        };

        let arrival_index: Option<usize> = match d.get_item("arrival_index")? {
            Some(v) => Some(v.extract()?),
            None => None,
        };

        out.push(Intent { profit, resources, client_id, arrival_index });
    }
    Ok(out)
}

fn fifo_select(mut items: Vec<Intent>) -> Vec<Intent> {
    items.sort_by_key(|i| i.arrival_index.unwrap_or(usize::MAX));
    let mut selected: Vec<Intent> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for it in items.into_iter() {
        if it.resources.iter().all(|r| !used.contains(r)) {
            used.extend(it.resources.iter().cloned());
            selected.push(it);
        }
    }
    selected
}

fn greedy_select(mut items: Vec<Intent>) -> Vec<Intent> {
    items.sort_by(|a, b| b.profit.partial_cmp(&a.profit).unwrap_or(std::cmp::Ordering::Equal));
    let mut selected: Vec<Intent> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for it in items.into_iter() {
        if it.resources.iter().all(|r| !used.contains(r)) {
            used.extend(it.resources.iter().cloned());
            selected.push(it);
        }
    }
    selected
}

fn total_profit(items: &[Intent]) -> f64 {
    items.iter().map(|i| if i.profit > 0.0 { i.profit } else { 0.0 }).sum()
}

fn intent_to_pydict<'py>(py: Python<'py>, it: &Intent) -> PyResult<&'py PyDict> {
    let d = PyDict::new(py);
    d.set_item("profit", it.profit)?;
    d.set_item("resources", &it.resources)?;
    match &it.client_id {
        Some(cid) => d.set_item("client_id", cid)?,
        None => d.set_item("client_id", py.None())?,
    }
    match it.arrival_index {
        Some(ai) => d.set_item("arrival_index", ai)?,
        None => d.set_item("arrival_index", py.None())?,
    }
    Ok(d)
}

#[pyfunction]
fn build_block(py: Python<'_>, intents: &PyAny, algorithm: &str) -> PyResult<(Vec<PyObject>, f64, String)> {
    let items = parse_intents(intents)?;
    let selected = match algorithm {
        "fifo" => fifo_select(items),
        _ => greedy_select(items),
    };
    let profit = total_profit(&selected);
    let mut out: Vec<PyObject> = Vec::with_capacity(selected.len());
    for it in selected.iter() {
        let d = intent_to_pydict(py, it)?;
        out.push(d.into_py(py));
    }
    Ok((out, profit, algorithm.to_string()))
}

#[pymodule]
fn fpn_engine(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(build_block, m)?)?;
    Ok(())
}
