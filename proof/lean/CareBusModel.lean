/-!
# CareBusModel — a model of the visitor-facing parts of `compose_reply`

This file is checked by `lean4-prover` (`POST /api/check`). It mirrors two pure
Rust functions in `crates/carebus/src/interaction.rs`:

* `without_citation_markers(answer, bound)` — removes the citation markers the
  answer actually cites (`[1]`, `[1][2]`, `[1, 2]`), keeps a bracket the answer
  does not cite (`[2026]`, a marker above the bound), and drops the space that
  separated the marker from its neighbours.
* the `faq` arm of `compose_reply` — an answer is shown only when it cites at
  least one source and has non-blank text; otherwise the fallback is shown.

The model is on `List Char`, not `String`, so the kernel can reduce it with
`decide` and no Mathlib import is needed. It is a MODEL: it does not prove the
Rust implementation, and the escape conversion (`unescaped_newlines`) is out of
scope. `NegativeControl.lean` is the non-vacuity check.
-/

namespace CareBus

/-- The ASCII whitespace `str::trim` removes in practice for a model answer.
Unicode whitespace is deliberately not modelled. -/
def isSpace (c : Char) : Bool :=
  c == ' ' || c == '\n' || c == '\r' || c == '\t'

def trimLeft : List Char → List Char
  | [] => []
  | c :: cs => if isSpace c then trimLeft cs else c :: cs

def trimRight (l : List Char) : List Char :=
  (trimLeft l.reverse).reverse

/-- `str::trim`, on the ASCII whitespace above. -/
def trim (l : List Char) : List Char :=
  trimRight (trimLeft l)

/-- One ASCII decimal digit. `Char.toNat` is used because Lean has no `≤` on `Char`. -/
def digitVal (c : Char) : Option Nat :=
  let n := c.toNat
  if decide (48 ≤ n ∧ n ≤ 57) then some (n - 48) else none

/-- `str::parse::<usize>()` for a non-empty all-ASCII-digit list. -/
def parseNat (l : List Char) : Option Nat :=
  if l.isEmpty then none
  else
    l.foldl
      (fun acc c =>
        match acc, digitVal c with
        | some n, some d => some (n * 10 + d)
        | _, _ => none)
      (some 0)

/-- `split(',')`, keeping empty parts (Rust's `str::split` does). -/
def splitComma (l : List Char) : List (List Char) :=
  let rec go (cur : List Char) (acc : List (List Char)) : List Char → List (List Char)
    | [] => acc ++ [cur.reverse]
    | ',' :: rest => go [] (acc ++ [cur.reverse]) rest
    | c :: rest => go (c :: cur) acc rest
  go [] [] l

/-- The Rust closure: every comma-separated, trimmed part parses as a number in
`1..=max`. An empty part does not count, matching `parse::<usize>()` failing. -/
def cited (max : Nat) (inner : List Char) : Bool :=
  let parts := splitComma inner
  !parts.isEmpty
    && parts.all (fun p =>
      match parseNat (trim p) with
      | some n => decide (1 ≤ n ∧ n ≤ max)
      | none => false)

/-- `str::split_once(']')` on the tail: return the text before the first `]` and
the text after it, or `none` when no `]` remains. -/
def takeUntilClose (acc : List Char) : List Char → Option (List Char × List Char)
  | [] => none
  | ']' :: rest => some (acc.reverse, rest)
  | c :: rest => takeUntilClose (c :: acc) rest

/-- The Rust `if out.ends_with(' ') { out.pop() }`: drop ONE trailing space. -/
def dropTrailingSpace (l : List Char) : List Char :=
  match l.reverse with
  | ' ' :: rest => rest.reverse
  | _ => l

/-- The scan, with the caller's fuel standing in for the Rust loop's `while let`.
`out` is the text emitted so far in normal order. One input character (or the
whole `[inner]`) is consumed per step, so `text.length + 1` fuel is always enough. -/
def stripGo (max : Nat) : Nat → List Char → List Char → List Char
  | 0, out, rest => out ++ rest
  | Nat.succ _, out, [] => out
  | Nat.succ n, out, c :: rest =>
      if c = '[' then
        match takeUntilClose [] rest with
        | some (inner, after) =>
            if cited max inner then
              stripGo max n (dropTrailingSpace out) after
            else
              stripGo max n (out ++ ['[']) rest
        | none => out ++ ('[' :: rest)
      else
        stripGo max n (out ++ [c]) rest

/-- `without_citation_markers`: the scan followed by the Rust `trim()`. -/
def strip (max : Nat) (text : List Char) : List Char :=
  trim (stripGo max (text.length + 1) [] text)

/-- The Rust `bound`: the highest per-document `number` when the sources carry
one, else the source count when there are sources, else the deduped cited count. -/
def maxOpt : List Nat → Option Nat
  | [] => none
  | n :: ns => some (ns.foldl max n)

def sourcesBound (numbers : List (Option Nat)) (citedCount : Nat) : Nat :=
  let present := numbers.filterMap id
  match maxOpt present with
  | some n => n
  | none => if numbers.isEmpty then citedCount else numbers.length

/-- The reply a visitor reads: the fallback, or the model answer with its markers
removed. The exact fallback text is not part of the proof; the branch is. -/
inductive Reply where
  | fallback
  | answer (text : List Char)
  deriving DecidableEq, Repr

/-- The `faq` arm of `compose_reply`, without `unescaped_newlines` (out of scope). -/
def composeFaq
    (numbers : List (Option Nat))
    (citations : List Nat)
    (answer : Option (List Char)) : Reply :=
  let bound := sourcesBound numbers citations.length
  match answer with
  | none => Reply.fallback
  | some a =>
      if citations.isEmpty then Reply.fallback
      else if (trim a).isEmpty then Reply.fallback
      else Reply.answer (strip bound a)

/-! ## Length: the stripper never grows the text -/

theorem trimLeft_length_le (l : List Char) : (trimLeft l).length ≤ l.length := by
  induction l with
  | nil => simp [trimLeft]
  | cons c cs ih =>
      unfold trimLeft
      split
      · simp only [List.length_cons]
        omega
      · simp

theorem trimRight_length_le (l : List Char) : (trimRight l).length ≤ l.length := by
  unfold trimRight
  have h := trimLeft_length_le l.reverse
  rw [List.length_reverse] at h
  rw [List.length_reverse]
  exact h

theorem trim_length_le (l : List Char) : (trim l).length ≤ l.length := by
  unfold trim
  exact Nat.le_trans (trimRight_length_le _) (trimLeft_length_le _)

theorem dropTrailingSpace_length_le (l : List Char) :
    (dropTrailingSpace l).length ≤ l.length := by
  unfold dropTrailingSpace
  split
  · rename_i rest heq
    have hlen : l.length = rest.length + 1 := by
      rw [← List.length_reverse, heq]
      simp
    rw [List.length_reverse]
    omega
  · exact Nat.le_refl _

theorem takeUntilClose_snd_length_le (acc : List Char) (l : List Char)
    (inner after : List Char) (h : takeUntilClose acc l = some (inner, after)) :
    after.length ≤ l.length := by
  induction l generalizing acc with
  | nil => simp [takeUntilClose] at h
  | cons c cs ih =>
      by_cases hc : c = ']'
      · subst hc
        simp [takeUntilClose] at h
        obtain ⟨rfl, rfl⟩ := h
        simp
      · have h' : takeUntilClose (c :: acc) cs = some (inner, after) := by
          simpa [takeUntilClose, hc] using h
        have ih' := ih (c :: acc) h'
        simp only [List.length_cons]
        omega

theorem stripGo_length_le (max fuel : Nat) (out rest : List Char)
    (h : rest.length ≤ fuel) :
    (stripGo max fuel out rest).length ≤ out.length + rest.length := by
  induction fuel generalizing out rest with
  | zero =>
      have hnil : rest = [] := List.eq_nil_of_length_eq_zero (by omega)
      subst hnil
      simp [stripGo]
  | succ n ih =>
      cases rest with
      | nil => simp [stripGo]
      | cons c cs =>
          simp only [stripGo]
          by_cases hc : c = '['
          · simp only [hc, if_true]
            cases htc : takeUntilClose [] cs with
            | none =>
                simp only [List.length_append, List.length_cons] at h ⊢
                omega
            | some p =>
                obtain ⟨inner, after⟩ := p
                by_cases hcited : cited max inner = true
                · simp [hcited]
                  have hcs : cs.length ≤ n := by
                    simp only [List.length_cons] at h
                    omega
                  have hafter_cs := takeUntilClose_snd_length_le [] cs inner after htc
                  have hafter : after.length ≤ n := by omega
                  have hout := dropTrailingSpace_length_le out
                  have hrec := ih (dropTrailingSpace out) after hafter
                  omega
                · simp [hcited]
                  have hcs : cs.length ≤ n := by
                    simp only [List.length_cons] at h
                    omega
                  have hrec := ih (out ++ ['[']) cs hcs
                  simp only [List.length_append, List.length_cons, List.length_nil] at hrec ⊢
                  omega
          · simp only [hc, if_false]
            have hcs : cs.length ≤ n := by
              simp only [List.length_cons] at h
              omega
            have hrec := ih (out ++ [c]) cs hcs
            simp only [List.length_append, List.length_cons, List.length_nil] at hrec ⊢
            omega

theorem strip_length_le (max : Nat) (text : List Char) :
    (strip max text).length ≤ text.length := by
  unfold strip
  have hscan : (stripGo max (text.length + 1) [] text).length ≤ text.length := by
    have h := stripGo_length_le max (text.length + 1) [] text (by omega)
    simp only [List.length_nil] at h
    omega
  exact Nat.le_trans (trim_length_le _) hscan

/-! ## Conformance: the shapes the Rust unit tests pin -/

/-- `"word[1] next"` → `"word next"`. -/
example : strip 1 ['w','o','r','d','[','1',']',' ','n','e','x','t']
    = ['w','o','r','d',' ','n','e','x','t'] := by decide

/-- `"word [1] next"` → `"word next"`: the space before the marker goes with it. -/
example : strip 1 ['w','o','r','d',' ','[','1',']',' ','n','e','x','t']
    = ['w','o','r','d',' ','n','e','x','t'] := by decide

/-- `"[1] Leads the sentence"` → `"Leads the sentence"`. -/
example : strip 1
    ['[','1',']',' ','L','e','a','d','s',' ','t','h','e',' ','s','e','n','t','e','n','c','e']
    = ['L','e','a','d','s',' ','t','h','e',' ','s','e','n','t','e','n','c','e'] := by decide

/-- A run collapses: `"channels [1][2][3][4]. Prices are per seat [2]."` with
four sources. -/
example : strip 4
    ('C' :: 'a' :: 'r' :: 'e' :: ' ' :: 'B' :: 'u' :: 's' :: ' ' :: 'r' :: 'u' :: 'n' :: 's'
     :: ' ' :: 'i' :: 'n' :: ' ' :: 'y' :: 'o' :: 'u' :: 'r' :: ' ' :: 'o' :: 'w' :: 'n'
     :: ' ' :: 'c' :: 'h' :: 'a' :: 'n' :: 'n' :: 'e' :: 'l' :: 's' :: ' ' :: '[' :: '1'
     :: ']' :: '[' :: '2' :: ']' :: '[' :: '3' :: ']' :: '[' :: '4' :: ']' :: '.' :: ' '
     :: 'P' :: 'r' :: 'i' :: 'c' :: 'e' :: 's' :: ' ' :: 'a' :: 'r' :: 'e' :: ' ' :: 'p'
     :: 'e' :: 'r' :: ' ' :: 's' :: 'e' :: 'a' :: 't' :: ' ' :: '[' :: '2' :: ']' :: '.'
     :: [])
    = ('C' :: 'a' :: 'r' :: 'e' :: ' ' :: 'B' :: 'u' :: 's' :: ' ' :: 'r' :: 'u' :: 'n' :: 's'
     :: ' ' :: 'i' :: 'n' :: ' ' :: 'y' :: 'o' :: 'u' :: 'r' :: ' ' :: 'o' :: 'w' :: 'n'
     :: ' ' :: 'c' :: 'h' :: 'a' :: 'n' :: 'n' :: 'e' :: 'l' :: 's' :: '.' :: ' ' :: 'P'
     :: 'r' :: 'i' :: 'c' :: 'e' :: 's' :: ' ' :: 'a' :: 'r' :: 'e' :: ' ' :: 'p' :: 'e'
     :: 'r' :: ' ' :: 's' :: 'e' :: 'a' :: 't' :: '.'
     :: []) := by decide

/-- A comma list (`[1, 2]`) is the other shape the models produce. -/
example : strip 2 ['a',' ','[','1',',',' ','2',']',' ','b'] = ['a',' ','b'] := by decide

/-- A bracket above the bound is the prose's own, not a citation: `[2026]` stays. -/
example : strip 4 ['T','h','e',' ','2','0','2','6',' ','[','5',']',' ','r','e','v','i','e','w']
    = ['T','h','e',' ','2','0','2','6',' ','[','5',']',' ','r','e','v','i','e','w'] := by decide

/-- Nested brackets, exactly the Rust test vector `"a [x] and [[2]] b [9]"`. -/
example : strip 3
    ['a',' ','[','x',']',' ','a','n','d',' ','[','[','2',']',']',' ','b',' ','[','9',']']
    = ['a',' ','[','x',']',' ','a','n','d',' ','[',']',' ','b',' ','[','9',']'] := by decide

/-- The boundary, stated rather than hidden: adjacent brackets can synthesise a
new marker, so the one-pass stripper is NOT idempotent on every input. The
citation shapes the models produce never place a `[` directly against another
`[`. `NegativeControl.lean` checks that a false claim is refused. -/
example : strip 2 ['[','[','1',']','2',']'] = ['[','2',']'] := by decide

/-! ## The faq reply's grounding branch -/

/-- No citations: the fallback, whatever the answer says. -/
example (numbers : List (Option Nat)) (a : List Char) :
    composeFaq numbers [] (some a) = Reply.fallback := by simp [composeFaq]

/-- No answer: the fallback. -/
example (numbers : List (Option Nat)) (citations : List Nat) :
    composeFaq numbers citations none = Reply.fallback := by simp [composeFaq]

/-- A blank answer: the fallback even with citations. -/
example (numbers : List (Option Nat)) (citations : List Nat) (a : List Char)
    (h : trim a = []) :
    composeFaq numbers citations (some a) = Reply.fallback := by
  simp [composeFaq, h]

/-- A cited, non-blank answer is shown with its marker removed. -/
example :
    composeFaq [some 1] [7] (some ['H','i',' ','[','1',']']) = Reply.answer ['H','i'] := by
  decide

/-! ## Axiom audit

`POST /api/check` answers `valid: true` for exit 0 with no error, and a proof hole is only a
warning. The runner parses these lines and refuses `sorryAx` (and any axiom outside the three Lean
core admits). A theorem renamed here without updating `proof/run.mjs` fails the check. -/

#print axioms trimLeft_length_le
#print axioms trimRight_length_le
#print axioms trim_length_le
#print axioms dropTrailingSpace_length_le
#print axioms takeUntilClose_snd_length_le
#print axioms stripGo_length_le
#print axioms strip_length_le

end CareBus
