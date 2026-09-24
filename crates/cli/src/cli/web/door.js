// The credential's lifecycle, as one state machine both pages drive.
//
// Every bug this exists to stop was a bug about ORDER, not about a
// value: a recoverable failure that could not be retried, a sign-in
// that succeeded and looped, a sign-out that failed and cleared
// anyway, a provisioning reply that landed after a forget and put the
// credential back. All four were found by a reviewer reading the
// script (`the-phone-scans-the-token-and-keeps-it`).
//
// So the shape here is events in, state and effects out. The pages
// hold one state object, dispatch what happened, and perform what
// comes back; no DOM, no fetch, no storage on this side. A page
// cannot snapshot a flag and decide on it later, because it never
// passes flags — it passes events, and the step reads the state as it
// is at that moment. That is the provisioning race made unwritable
// rather than merely tested.
//
// Inlined into both pages at their door anchor, so this text is what
// ships; the tests run this file. The anchor itself is not spelled
// here: substitution would put it straight back and the page would
// carry a marker that reads as already-filled.

// What a sign-in attempt MEANT, from the reply alone.
//
// `reply` is null when nothing came back. Otherwise it carries `ok`,
// the status, and the parsed JSON body — or a null body for one that
// was not JSON. The page does the awaiting and the parsing.
//
// 403 alone cannot mean the credential is dead: a request from another
// origin is refused with it too, and only a refusal naming the token
// says what is stored has stopped being a way in. Throwing the token
// away over a rate limit or a store failure turns a wait into a paste.
function signInVerdict(reply) {
  if (!reply) return 'unreachable';
  if (reply.ok) return 'in';
  if (reply.status === 403 && reply.body && reply.body.refused === 'token') return 'dead';
  return 'refused';
}

// The state each page starts from. `stored` is the remembered
// credential, `tried` the one-shot guard.
//
// `sealed` is the door: set the moment a sign-out is asked for, and
// cleared ONLY if the door refuses. It is deliberately TERMINAL on a
// sign-out that succeeded — a reply still in flight from before the
// forget may land afterwards, and nothing may write the credential
// back. It is not named `forgetting` for that reason: a flag named
// after an operation invites being cleared when the operation ends,
// which silently re-arms the race this whole module exists to stop.
function doorStart(env) {
  return {
    stored: (env && env.stored) || null,
    tried: !!(env && env.tried),
    sealed: false,
  };
}

// One event, producing the next state and what to do about it.
//
// Effects are named, not performed: `signIn`, `store`, `discard`,
// `guard`, `navigate`, `say`. The page maps them onto fetch, storage
// and the DOM, and maps a `say` reason onto words.
function doorStep(state, event) {
  const s = {
    stored: state.stored,
    tried: state.tried,
    sealed: state.sealed,
  };
  const fx = [];
  const out = () => ({ state: s, effects: fx });

  switch (event && event.t) {
    // A returning browser signs itself back in, so a returning phone
    // is not a form — but only while the one-shot guard is unspent.
    case 'load':
      if (s.stored && !s.tried) fx.push({ do: 'signIn', token: s.stored });
      return out();

    case 'signInReply': {
      const verdict = signInVerdict(event.reply);
      // The guard is spent ONLY by a sign-in that succeeded and came
      // back here anyway: that is the one unrecoverable shape — a
      // cookie the browser refuses to keep — and retrying it loops
      // between the two pages. Everything else leaves it alone, so
      // the next load tries again.
      if (verdict === 'in') {
        s.tried = true;
        fx.push({ do: 'guard', set: true }, { do: 'navigate', to: '/' });
        return out();
      }
      if (verdict === 'dead') {
        s.stored = null;
        fx.push({ do: 'discard' });
      }
      fx.push({ do: 'say', why: verdict });
      return out();
    }

    // Arriving signed in IS the confirmation the cookie stuck, so the
    // guard is released: a cookie lost later is signed back in rather
    // than left at the form.
    case 'signedIn':
      s.tried = false;
      fx.push({ do: 'guard', set: false });
      return out();

    case 'forget':
      if (s.sealed) return out();
      s.sealed = true;
      fx.push({ do: 'forget' });
      return out();

    // A sign-out the door refused is not a sign-out: the cookie is
    // still live and would fetch the credential straight back, so
    // clearing the store or navigating would keep nothing while
    // claiming to. The three parts move together or not at all.
    case 'forgetReply':
      if (!event.ok) {
        s.sealed = false;
        fx.push({ do: 'say', why: 'forgetFailed' });
        return out();
      }
      // `s.sealed` STAYS set: see `doorStart`. The page navigates,
      // but a provisioning reply already in flight can still resolve
      // first, and this is the only thing that stops it.
      s.stored = null;
      fx.push({ do: 'discard' }, { do: 'navigate', to: '/login' });
      return out();

    // The fetch this answers for was in flight when the forget began,
    // so it lands after it. `s.sealed` is read HERE, at the moment
    // of writing, which is the whole point of passing an event rather
    // than a flag the caller captured earlier.
    case 'provisionReply':
      if (event.token && !s.sealed) {
        s.stored = event.token;
        fx.push({ do: 'store', token: event.token });
      }
      return out();
  }
  return out();
}
