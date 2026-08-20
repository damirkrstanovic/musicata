// SPDX-License-Identifier: AGPL-3.0-or-later
// The live search query, shared between the header input and the results view so typing
// refetches in place (the route stays "search") rather than remounting per keystroke.
class Search {
  query = $state("");
}

export const search = new Search();
