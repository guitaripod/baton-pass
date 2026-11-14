use super::link::{Incoming, Link};
use crate::config::{Config, Side};
use crate::crypto::Cipher;
use crate::proto::Msg;
use anyhow::{Result, bail};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Links with the Mac without touching local input, reports round-trip times and, with `moves`,
/// drives the Mac cursor 200 pixels' worth to the left so the whole injection path can be checked
/// headlessly. `hotkey` enters without moving the cursor to the edge first.
#[derive(clap::Args)]
pub struct ProbeArgs {
    /// Heartbeat round trips to time
    #[arg(long, default_value_t = 200)]
    samples: usize,
    /// Send 200 pixels of leftward motion
    #[arg(long)]
    moves: bool,
    /// Pixels per motion packet
    #[arg(long, default_value_t = 2.0)]
    step: f32,
    /// Enter like the hotkey does, leaving the Mac cursor where it is
    #[arg(long)]
    hotkey: bool,
    /// Push right until the Mac reports its edge was crossed
    #[arg(long)]
    cross: bool,
    /// Send three wheel notches of this many 1/120ths (positive is away from you)
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    wheel: i32,
    /// Hold Shift and Command, then leave without releasing them
    #[arg(long)]
    shift: bool,
    /// Put this text on the Mac clipboard, then restore what was there
    #[arg(long)]
    clip: Option<String>,
}

pub async fn probe(args: ProbeArgs) -> Result<()> {
    let ProbeArgs { samples, moves, step, hotkey, cross, wheel, shift, clip } = args;
    let config = Config::load()?;
    let cipher = Rc::new(Cipher::load(&Config::key_path())?);
    let mut link = Link::new(cipher, config.peer_addresses()).await?;
    let mut buffer = vec![0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(5);
    while !link.connected() {
        if Instant::now() > deadline {
            bail!("no answer from {}", config.peer_addresses().join(", "));
        }
        link.maintain().await;
        if let Ok(Ok((length, from))) = tokio::time::timeout(Duration::from_millis(50), link.recv(&mut buffer)).await {
            link.accept(&buffer[..length], from);
        }
    }
    println!("linked with {}", link.peer.expect("connected"));

    let mut rtts = Vec::with_capacity(samples);
    for _ in 0..samples {
        let sent = Instant::now();
        link.send(Msg::Heartbeat { entered: false, keys: Default::default(), buttons: 0 }).await;
        let reply = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let (length, from) = link.recv(&mut buffer).await?;
                if let Some(Incoming::Message(Msg::Ack { flags })) = link.accept(&buffer[..length], from) {
                    return Ok::<u8, std::io::Error>(flags);
                }
            }
        })
        .await;
        match reply {
            Ok(Ok(flags)) => {
                rtts.push(sent.elapsed());
                link.flags = flags;
            }
            _ => println!("lost a heartbeat"),
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    rtts.sort();
    if !rtts.is_empty() {
        let pick = |q: f64| rtts[((rtts.len() - 1) as f64 * q) as usize].as_micros();
        println!(
            "round trip over {} heartbeats: min {} us, p50 {} us, p99 {} us, max {} us",
            rtts.len(),
            pick(0.0),
            pick(0.5),
            pick(0.99),
            pick(1.0)
        );
    }
    println!("mac flags: {:#04x}", link.flags);

    if moves {
        link.send(Msg::Enter { side: Side::Left.to_wire(), frac: if hotkey { f32::NAN } else { 0.5 }, speed: 1.0 }).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        link.send(Msg::Heartbeat { entered: true, keys: Default::default(), buttons: 0 }).await;
        let packets = (200.0 / step).round().max(1.0) as usize;
        for index in 0..packets {
            link.send(Msg::Pointer { dx: -step, dy: 0.0, wheel_v: 0, wheel_h: 0, buttons: 0 }).await;
            tokio::time::sleep(Duration::from_millis(2)).await;
            if index % 40 == 0 {
                link.send(Msg::Heartbeat { entered: true, keys: Default::default(), buttons: 0 }).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        link.send(Msg::Leave).await;
        println!("sent 200 pixels of leftward motion");
    }

    if wheel != 0 {
        link.send(Msg::Enter { side: Side::Left.to_wire(), frac: f32::NAN, speed: 1.0 }).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        for _ in 0..3 {
            link.send(Msg::Pointer { dx: 0.0, dy: 0.0, wheel_v: wheel, wheel_h: 0, buttons: 0 }).await;
            link.send(Msg::Heartbeat { entered: true, keys: Default::default(), buttons: 0 }).await;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        link.send(Msg::Leave).await;
        println!("sent three wheel notches of {wheel}");
    }

    if shift {
        link.send(Msg::Enter { side: Side::Left.to_wire(), frac: f32::NAN, speed: 1.0 }).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let held = crate::proto::KeyState { modifiers: 0x02 | 0x08, keys: Vec::new(), consumer: Vec::new() };
        link.send(Msg::Keys(held.clone())).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        link.send(Msg::Heartbeat { entered: true, keys: held, buttons: 0 }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        link.send(Msg::Leave).await;
        println!("held Shift+Command for 400 ms, then left without releasing them");
    }

    if let Some(text) = clip {
        let (Some(peer), Some(token)) = (link.peer, link.token) else { bail!("not linked") };
        let cipher = link.cipher();
        let mut counter = crate::crypto::Counter::new();
        let mut frame = |msg: Msg| cipher.seal(&crate::proto::Envelope { token, counter: counter.next(), msg }.encode());
        let saved = super::clipboard::exchange(&cipher, peer, frame(Msg::ClipGet), token).await?;
        let put = Msg::ClipPut { mime: "text/plain;charset=utf-8".into(), data: text.clone().into_bytes() };
        let started = Instant::now();
        super::clipboard::exchange(&cipher, peer, frame(put), token).await?;
        println!("put {text:?} on the Mac clipboard in {} ms", started.elapsed().as_millis());
        let again = super::clipboard::exchange(&cipher, peer, frame(Msg::ClipGet), token).await?;
        println!("asking again returns {}", if matches!(again.msg, Msg::ClipNone) { "nothing (unchanged)" } else { "data" });
        if let Msg::ClipData { mime, data } = saved.msg {
            println!("restoring the previous {mime} ({} bytes)", data.len());
            super::clipboard::exchange(&cipher, peer, frame(Msg::ClipPut { mime, data }), token).await?;
        }
    }

    if cross {
        link.send(Msg::Enter { side: Side::Left.to_wire(), frac: 0.25, speed: 1.0 }).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = Instant::now();
        let mut exit = None;
        while exit.is_none() && started.elapsed() < Duration::from_secs(2) {
            link.send(Msg::Pointer { dx: 4.0, dy: 0.0, wheel_v: 0, wheel_h: 0, buttons: 0 }).await;
            link.send(Msg::Heartbeat { entered: true, keys: Default::default(), buttons: 0 }).await;
            if let Ok(Ok((length, from))) = tokio::time::timeout(Duration::from_millis(5), link.recv(&mut buffer)).await
                && let Some(Incoming::Message(Msg::Exit { frac })) = link.accept(&buffer[..length], from) {
                    exit = Some((frac, started.elapsed()));
                }
        }
        link.send(Msg::Leave).await;
        match exit {
            Some((frac, after)) => println!("the Mac reported an edge crossing at {frac:.3} after {} ms", after.as_millis()),
            None => bail!("no edge crossing reported"),
        }
    }
    Ok(())
}
