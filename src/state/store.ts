type Listener = () => void
export type OnChange<T> = (args: {
  newState: T
  oldState: T
}) => void | false

export type Store<T> = {
  getState: () => T
  /** Returns false only when the persistence/change guard rejects the update. */
  setState: (updater: (prev: T) => T) => boolean
  subscribe: (listener: Listener) => () => void
}

export function createStore<T>(
  initialState: T,
  onChange?: OnChange<T>,
): Store<T> {
  let state = initialState
  const listeners = new Set<Listener>()

  return {
    getState: () => state,

    setState: (updater: (prev: T) => T) => {
      const prev = state
      const next = updater(prev)
      if (Object.is(next, prev)) return true
      state = next
      const accepted = onChange?.({ newState: next, oldState: prev })
      if (accepted === false) {
        // A synchronous persistence guard (currently custom-model selection)
        // can reject the transition before subscribers observe it.
        state = prev
        return false
      }
      for (const listener of listeners) listener()
      return true
    },

    subscribe: (listener: Listener) => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
  }
}
