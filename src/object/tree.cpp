#include "tree.h"
#include "tagged.h"
#include "any.h"
#include "store.h"

#include "../hex.h"
#include "../array_io.h"

#include <boost/archive/text_oarchive.hpp>
#include <iostream>
#include <boost/filesystem.hpp>

using namespace ouisync;
using namespace ouisync::object;

Sha256::Digest Tree::calculate_digest() const
{
    Sha256 hash;
    for (auto& [k,v] : *this) {
        hash.update(k);
        hash.update(v);
    }
    return hash.close();
}

Id Tree::store(const fs::path& root) const
{
    return object::store(root, *this);
}
