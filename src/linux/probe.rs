use super::link::{Incoming, Link};
use super::{HEARTBEAT_CAPTURED, KeyRefresh};
use crate::proto::KeyState;
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
    /// Tap F20, which nothing on a Mac answers, this many times for 90 ms each, the way the
    /// daemon sends keys; the Mac's log then says how many it had to let go of early
    #[arg(long, default_value_t = 0)]
    taps: usize,
}

/// F20 on the HID keyboard page: a real key that macOS passes on and nothing binds.
const F20: u16 = 0x6f;

/// Taps `usage` `taps` times, held for `hold` each, one tap every 230 ms or more so taps meet a
/// periodic stall at every phase. Key changes go out at once and held keys are restated with
/// `KeyRefresh`, heartbeats every `HEARTBEAT_CAPTURED`, and replies are read as they come, all
/// as the daemon does while captured.
pub(crate) async fn tap(link: &mut Link, usage: u16, taps: usize, hold: Duration) {
    let gap = Duration::from_millis(230).max(hold + Duration::from_millis(140));
    let keys = |down: bool| KeyState { modifiers: 0, keys: if down { vec![usage] } else { Vec::new() }, consumer: Vec::new() };
    let started = Instant::now();
    let end = gap * taps as u32 + Duration::from_millis(400);
    let mut pressed = false;
    let mut refresh = KeyRefresh::default();
    let mut last_heartbeat = Instant::now();
    let mut buffer = [0u8; 2048];
    while started.elapsed() < end {
        let elapsed = started.elapsed();
        let index = (elapsed.as_millis() / gap.as_millis()) as usize;
        let wanted = index < taps && elapsed - gap * (index as u32) < hold;
        if wanted != pressed {
            pressed = wanted;
            let at = link.clock();
            link.send(Msg::Keys { keys: keys(pressed), at }).await;
            refresh.changed();
        }
        if refresh.due().is_some_and(|due| Instant::now() >= due) {
            refresh.sent(&keys(pressed));
            let at = link.clock();
            link.send(Msg::Keys { keys: keys(pressed), at }).await;
        }
        if last_heartbeat.elapsed() >= HEARTBEAT_CAPTURED {
            last_heartbeat = Instant::now();
            let at = link.clock();
            link.send(Msg::Heartbeat { entered: true, keys: keys(pressed), buttons: 0, at }).await;
        }
        while let Ok((size, from)) = link.try_recv(&mut buffer) {
            link.accept(&buffer[..size], from).await;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

pub async fn probe(args: ProbeArgs) -> Result<()> {
    let ProbeArgs { samples, moves, step, hotkey, cross, wheel, shift, clip, taps } = args;
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
            link.accept(&buffer[..length], from).await;
        }
    }
    println!("linked with {}", link.peer.expect("connected"));

    let mut rtts = Vec::with_capacity(samples);
    for _ in 0..samples {
        let sent = Instant::now();
        link.send(Msg::Heartbeat { entered: false, keys: Default::default(), buttons: 0, at: link.clock() }).await;
        let reply = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let (length, from) = link.recv(&mut buffer).await?;
                if let Some(Incoming::Message(Msg::Ack { flags, .. })) = link.accept(&buffer[..length], from).await {
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
        link.send(Msg::Enter { side: Side::Left.to_wire(), frac: if hotkey { f32::NAN } else { 0.5 }, speed: 1.0, exit: true }).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        link.send(Msg::Heartbeat { entered: true, keys: Default::default(), buttons: 0, at: link.clock() }).await;
        let packets = (200.0 / step).round().max(1.0) as usize;
        for index in 0..packets {
            link.send(Msg::Pointer { dx: -step, dy: 0.0, wheel_v: 0, wheel_h: 0, buttons: 0, at: link.clock() }).await;
            tokio::time::sleep(Duration::from_millis(2)).await;
            if index % 40 == 0 {
                link.send(Msg::Heartbeat { entered: true, keys: Default::default(), buttons: 0, at: link.clock() }).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        link.send(Msg::Leave).await;
        println!("sent 200 pixels of leftward motion");
    }

    if wheel != 0 {
        link.send(Msg::Enter { side: Side::Left.to_wire(), frac: f32::NAN, speed: 1.0, exit: true }).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        for _ in 0..3 {
            link.send(Msg::Pointer { dx: 0.0, dy: 0.0, wheel_v: wheel, wheel_h: 0, buttons: 0, at: link.clock() }).await;
            link.send(Msg::Heartbeat { entered: true, keys: Default::default(), buttons: 0, at: link.clock() }).await;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        link.send(Msg::Leave).await;
        println!("sent three wheel notches of {wheel}");
    }

    if shift {
        link.send(Msg::Enter { side: Side::Left.to_wire(), frac: f32::NAN, speed: 1.0, exit: true }).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let held = crate::proto::KeyState { modifiers: 0x02 | 0x08, keys: Vec::new(), consumer: Vec::new() };
        link.send(Msg::Keys { keys: held.clone(), at: link.clock() }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        link.send(Msg::Heartbeat { entered: true, keys: held, buttons: 0, at: link.clock() }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        link.send(Msg::Leave).await;
        println!("held Shift+Command for 400 ms, then left without releasing them");
    }

    if taps > 0 {
        link.send(Msg::Enter { side: Side::Left.to_wire(), frac: f32::NAN, speed: 1.0, exit: true }).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        tap(&mut link, F20, taps, Duration::from_millis(90)).await;
        link.send(Msg::Leave).await;
        println!("tapped F20 {taps} times for 90 ms each");
    }

    if let Some(text) = clip {
        let (Some(peer), Some(token)) = (link.peer, link.token) else { bail!("not linked") };
        let cipher = link.cipher();
        let mut counter = crate::crypto::Counter::new();
        let mut frame = |msg: Msg| cipher.seal(&crate::proto::Envelope { token, counter: counter.next(), msg }.encode());
        let saved = super::clipboard::exchange(&cipher, peer, frame(Msg::ClipGet { have: [0; 32] }), token).await?;
        let test: crate::clip::Clip = vec![
            (crate::clip::HTML.to_string(), format!("<b>{text}</b>").into_bytes()),
            (crate::clip::TEXT.to_string(), text.clone().into_bytes()),
        ];
        let put = Msg::ClipPut { clip: test.clone() };
        let started = Instant::now();
        super::clipboard::exchange(&cipher, peer, frame(put), token).await?;
        println!("put {text:?} on the Mac clipboard in {} ms", started.elapsed().as_millis());
        let back = super::clipboard::exchange(&cipher, peer, frame(Msg::ClipGet { have: [0; 32] }), token).await?;
        match &back.msg {
            Msg::ClipData { clip, .. } => println!("the Mac now offers {}", crate::clip::describe(clip)),
            _ => println!("the Mac offered nothing back"),
        }
        let have = crate::clip::digest(&test);
        let again = super::clipboard::exchange(&cipher, peer, frame(Msg::ClipGet { have }), token).await?;
        println!(
            "asking again with its digest returns {}",
            if matches!(again.msg, Msg::ClipNone { .. }) { "nothing (unchanged)" } else { "data" }
        );
        let restore = match saved.msg {
            Msg::ClipData { clip, .. } => Msg::ClipPut { clip },
            _ => Msg::ClipPut { clip: vec![(crate::clip::TEXT.to_string(), Vec::new())] },
        };
        super::clipboard::exchange(&cipher, peer, frame(restore), token).await?;
        println!("restored the Mac clipboard");
    }

    if cross {
        link.send(Msg::Enter { side: Side::Left.to_wire(), frac: 0.25, speed: 1.0, exit: true }).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        link.send(Msg::Pointer { dx: -40.0, dy: 0.0, wheel_v: 0, wheel_h: 0, buttons: 0, at: link.clock() }).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = Instant::now();
        let mut exit = None;
        while exit.is_none() && started.elapsed() < Duration::from_secs(2) {
            link.send(Msg::Pointer { dx: 4.0, dy: 0.0, wheel_v: 0, wheel_h: 0, buttons: 0, at: link.clock() }).await;
            link.send(Msg::Heartbeat { entered: true, keys: Default::default(), buttons: 0, at: link.clock() }).await;
            if let Ok(Ok((length, from))) = tokio::time::timeout(Duration::from_millis(5), link.recv(&mut buffer)).await
                && let Some(Incoming::Message(Msg::Exit { frac })) = link.accept(&buffer[..length], from).await {
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
