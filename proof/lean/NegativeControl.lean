/-!
# NegativeControl — the non-vacuity check for CareBusModel.lean

A kernel check that never refuses anything proves nothing. This file states a
claim that is false and lets `decide` fail, so `POST /api/check` must answer
`valid: false`. The proof Job treats a `valid: true` here as a failure of the
whole proof run.
-/

theorem carebus_negative_control : (2 : Nat) = 3 := by decide
