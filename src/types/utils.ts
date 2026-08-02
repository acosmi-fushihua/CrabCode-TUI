/**
 * Recursive deep-immutable wrapper.
 *
 * Converts every property (including nested Maps, Sets, and Arrays) to their
 * readonly counterparts so that the resulting type cannot be mutated.
 */
export type DeepImmutable<T> =
  T extends ReadonlyMap<infer K, infer V>
    ? ReadonlyMap<DeepImmutable<K>, DeepImmutable<V>>
    : T extends Map<infer K, infer V>
      ? ReadonlyMap<DeepImmutable<K>, DeepImmutable<V>>
      : T extends ReadonlySet<infer V>
        ? ReadonlySet<DeepImmutable<V>>
        : T extends Set<infer V>
          ? ReadonlySet<DeepImmutable<V>>
          : T extends Array<infer V>
            ? ReadonlyArray<DeepImmutable<V>>
            : T extends object
              ? { readonly [K in keyof T]: DeepImmutable<T[K]> }
              : T

/**
 * Produces a union of all possible tuple permutations of the members of union
 * type `T`.
 *
 * Example:
 *   Permutations<'a' | 'b'> = ['a', 'b'] | ['b', 'a']
 *
 * Useful with `satisfies` to guarantee exhaustive, order-independent coverage
 * of every union member in an array literal.
 */
export type Permutations<
  T,
  U = T,
> = [T] extends [never]
  ? []
  : U extends U
    ? [U, ...Permutations<Exclude<T, U>>]
    : never
