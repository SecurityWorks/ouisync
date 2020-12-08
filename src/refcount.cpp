#include "refcount.h"
#include "object/tree.h"
#include "object/blob.h"
#include "object/io.h"
#include "variant.h"
#include <boost/filesystem/fstream.hpp>
#include <boost/filesystem/operations.hpp>

#include <iostream>
#include <sstream>

namespace ouisync::refcount {

using object::Tree;
using object::Blob;

static
fs::path object_path(const ObjectId& id) noexcept {
    return object::path::from_id(id);
}

static
fs::path refcount_path(fs::path path) noexcept {
    path.concat(".rc");
    return path;
}

Number read(const fs::path& path_)
{
    auto path = refcount_path(path_);

    fs::fstream f(path, f.binary | f.in);
    if (!f.is_open()) {
        if (!fs::exists(path)) {
            // No one is holding this object
            return 0;
        }
        std::stringstream ss;
        ss << "Failed to read refcount: " << path;
        throw std::runtime_error(ss.str());
    }
    Number rc;
    f >> rc;
    return rc;
}

Number increment(const fs::path& path_)
{
    auto path = refcount_path(path_);

    fs::fstream f(path, f.binary | f.in | f.out);
    if (!f.is_open()) {
        // Does not exist, create a new one
        f.open(path, f.binary | f.out | f.trunc);
        if (!f.is_open()) {
            std::stringstream ss;
            ss << "Failed to increment refcount: " << path;
            throw std::runtime_error(ss.str());
        }
        f << 1 << '\n';
        return 1;
    }
    Number rc;
    f >> rc;
    ++rc;
    //std::cerr << "Refcount++ " << (rc-1) << " -> " << rc << " " << path << "\n";
    f.seekp(0);
    f << rc << '\n';
    return rc;
}

Number decrement(const fs::path& path_)
{
    auto path = refcount_path(path_);

    fs::fstream f(path, f.binary | f.in | f.out);
    if (!f.is_open()) {
        if (!fs::exists(path)) {
            // No one held this object
            return 0;
        }
        std::stringstream ss;
        ss << "Failed to decrement refcount: " << path;
        throw std::runtime_error(ss.str());
    }
    Number rc;
    f >> rc;
    if (rc == 0) throw std::runtime_error("Decrementing zero refcount");
    --rc;
    //std::cerr << "Refcount-- " << (rc+1) << " -> " << rc << " " << path << "\n";
    if (rc == 0) {
        f.close();
        fs::remove(path);
        return 0;
    }
    f.seekp(0);
    f << rc;
    return rc;
}

Number increment(const fs::path& objdir, const ObjectId& id)
{
    return increment(objdir / object_path(id));
}

Number decrement(const fs::path& objdir, const ObjectId& id)
{
    return decrement(objdir / object_path(id));
}

Number read(const fs::path& objdir, const ObjectId& id) {
    return read(objdir / object_path(id));
}

// -------------------------------------------------------------------

bool flat_remove(const fs::path& objdir, const ObjectId& id) {
    auto rc = refcount::decrement(objdir, id);
    if (rc > 0) return true;
    return object::io::remove(objdir, id);
}


void deep_remove(const fs::path& objdir, const ObjectId& id) {
    auto obj = object::io::load<Tree, Blob::Nothing>(objdir, id);

    apply(obj,
            [&](const Tree& tree) {
                for (auto& [name, id] : tree) {
                    (void)name; // https://stackoverflow.com/a/40714311/273348
                    deep_remove(objdir, id);
                }
            },
            [&](const Blob::Nothing&) {
            });

    flat_remove(objdir, id);
}

// -------------------------------------------------------------------

} // namespace