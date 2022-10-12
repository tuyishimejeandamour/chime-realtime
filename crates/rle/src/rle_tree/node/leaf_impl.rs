use crate::rle_tree::{
    cursor::SafeCursorMut,
    tree_trait::{FindPosResult, Position},
};
use std::fmt::{Debug, Error, Formatter};

use super::{utils::distribute, *};

impl<'bump, T: Rle, A: RleTreeTrait<T>> LeafNode<'bump, T, A> {
    #[inline]
    pub fn new(bump: &'bump Bump, parent: NonNull<InternalNode<'bump, T, A>>) -> Self {
        Self {
            bump,
            parent,
            children: BumpVec::with_capacity_in(A::MAX_CHILDREN_NUM, bump),
            prev: None,
            next: None,
            cache: Default::default(),
            _pin: PhantomPinned,
            _a: PhantomData,
        }
    }

    #[inline]
    fn _split<F>(&mut self, notify: &mut F) -> &'bump mut Node<'bump, T, A>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        let ans = self
            .bump
            .alloc(Node::Leaf(Self::new(self.bump, self.parent)));
        let ans_inner = ans.as_leaf_mut().unwrap();
        let ans_ptr = ans_inner as _;
        for child in self
            .children
            .drain(self.children.len() - A::MIN_CHILDREN_NUM..self.children.len())
        {
            notify(child, ans_ptr);
            ans_inner.children.push(child);
        }

        Self::connect(Some(ans_inner), self.next_mut());
        Self::connect(Some(self), Some(ans_inner));
        ans
    }

    #[inline]
    fn connect(a: Option<&mut LeafNode<'bump, T, A>>, b: Option<&mut LeafNode<'bump, T, A>>) {
        match (a, b) {
            (None, None) => {}
            (None, Some(next)) => next.prev = None,
            (Some(prev), None) => prev.next = None,
            (Some(a), Some(b)) => {
                a.next = Some(NonNull::new(b).unwrap());
                b.prev = Some(NonNull::new(a).unwrap());
            }
        }
    }

    #[inline]
    pub fn get_cursor<'tree>(&'tree self, pos: A::Int) -> SafeCursor<'bump, T, A> {
        let result = A::find_pos_leaf(self, pos);
        assert!(result.found);
        // SAFETY: result.found is true
        unsafe {
            SafeCursor::new(
                self.into(),
                result.child_index,
                result.offset,
                result.pos,
                0,
            )
        }
    }

    #[inline]
    pub fn get_cursor_mut<'b>(&'b mut self, pos: A::Int) -> SafeCursorMut<'bump, T, A> {
        let result = A::find_pos_leaf(self, pos);
        assert!(result.found);
        // SAFETY: result.found is true
        unsafe {
            SafeCursorMut::new(
                self.into(),
                result.child_index,
                result.offset,
                result.pos,
                0,
            )
        }
    }

    pub fn push_child<F>(
        &mut self,
        value: T,
        notify: &mut F,
    ) -> Result<(), &'bump mut Node<'bump, T, A>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        let self_ptr = self as *mut _;
        if !self.children.is_empty() {
            let last = self.children.last_mut().unwrap();
            if last.is_mergable(&value, &()) {
                last.merge(&value, &());
                notify(last, self_ptr);
                A::update_cache_leaf(self);
                return Ok(());
            }
        }

        if self.children.len() == A::MAX_CHILDREN_NUM {
            let ans = self._split(notify);
            let inner = ans.as_leaf_mut().unwrap();
            inner.push_child(value, notify).unwrap();
            A::update_cache_leaf(self);
            A::update_cache_leaf(inner);
            return Err(ans);
        }

        self.children.push(self.bump.alloc(value));
        notify(self.children[self.children.len() - 1], self_ptr);
        A::update_cache_leaf(self);
        Ok(())
    }

    pub(crate) fn check(&self) {
        assert!(self.children.len() <= A::MAX_CHILDREN_NUM);
        // assert!(self.children.len() >= A::MIN_CHILDREN_NUM);
        assert!(!self.is_deleted());
        A::check_cache_leaf(self);
        if let Some(next) = self.next {
            // SAFETY: this is only for testing, and next must be a valid pointer
            let self_ptr = unsafe { next.as_ref().prev.unwrap().as_ptr() };
            // SAFETY: this is only for testing, and next must be a valid pointer
            assert!(unsafe { !next.as_ref().is_deleted() });
            assert!(std::ptr::eq(self, self_ptr));
        }
        if let Some(prev) = self.prev {
            // SAFETY: this is only for testing, and prev must be a valid pointer
            let self_ptr = unsafe { prev.as_ref().next.unwrap().as_ptr() };
            // SAFETY: this is only for testing, and next must be a valid pointer
            assert!(unsafe { !prev.as_ref().is_deleted() });
            assert!(std::ptr::eq(self, self_ptr));
        }
    }

    fn _delete_start(&mut self, from: A::Int) -> (usize, Option<usize>) {
        let result = A::find_pos_leaf(self, from);
        match result.pos {
            Position::Start | Position::Before => (result.child_index, None),
            Position::Middle | Position::End | Position::After => {
                (result.child_index + 1, Some(result.offset))
            }
        }
    }

    fn _delete_end(&mut self, to: A::Int) -> (usize, Option<usize>) {
        let result = A::find_pos_leaf(self, to);
        match result.pos {
            Position::After | Position::End => (result.child_index + 1, None),
            Position::Start | Position::Middle | Position::Before => {
                (result.child_index, Some(result.offset))
            }
        }
    }

    pub fn is_deleted(&self) -> bool {
        // SAFETY: we used bumpalo here, so even if current node is deleted we
        unsafe {
            let mut node = self.parent.as_ref();
            if !node
                .children
                .iter()
                .any(|x| std::ptr::eq(x.as_leaf().unwrap(), self))
            {
                return true;
            }

            while let Some(parent) = node.parent {
                let parent = parent.as_ref();
                if !parent
                    .children()
                    .iter()
                    .any(|x| std::ptr::eq(x.as_internal().unwrap(), node))
                {
                    return true;
                }

                node = parent;
            }
        }

        false
    }

    pub fn insert<F>(
        &mut self,
        raw_index: A::Int,
        value: T,
        notify: &mut F,
    ) -> Result<(), &'bump mut Node<'bump, T, A>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        let result = {
            if self.children.is_empty() {
                notify(&value, self);
                self.children.push(self.bump.alloc(value));
                Ok(())
            } else {
                let FindPosResult {
                    child_index,
                    offset,
                    pos,
                    ..
                } = A::find_pos_leaf(self, raw_index);
                self._insert_at_pos(pos, child_index, offset, value, notify)
            }
        };
        self.with_cache_updated(result)
    }

    pub(crate) fn insert_at_pos<F>(
        &mut self,
        pos: Position,
        child_index: usize,
        offset: usize,
        value: T,
        notify: &mut F,
    ) -> Result<(), &'bump mut Node<'bump, T, A>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        let result = {
            if self.children.is_empty() {
                notify(&value, self);
                self.children.push(self.bump.alloc(value));
                Ok(())
            } else {
                self._insert_at_pos(pos, child_index, offset, value, notify)
            }
        };
        self.with_cache_updated(result)
    }

    /// update the content at given selection
    pub(crate) fn update_at_pos<F, U>(
        &mut self,
        pos: Position,
        child_index: usize,
        offset: usize,
        len: usize,
        update_fn: U,
        notify: &mut F,
    ) -> Result<(), &'bump mut Node<'bump, T, A>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
        U: FnOnce(&mut T),
    {
        if len == 0 {
            return Ok(());
        }

        if offset == 0 && self.children[child_index].content_len() == len {
            update_fn(self.children[child_index]);
            return Ok(());
        }

        let left = if offset == 0 {
            None
        } else {
            Some(self.children[child_index].slice(0, offset))
        };

        let right = if self.children[child_index].content_len() == offset + len {
            None
        } else {
            Some(
                self.children[child_index]
                    .slice(offset + len, self.children[child_index].content_len()),
            )
        };

        let mut target = self.children[child_index].slice(offset, offset + len);
        update_fn(&mut target);

        if let Some(left) = left {
            self.children[child_index] = self.bump.alloc(left);
            let left = &mut self.children[child_index];
            if left.is_mergable(&target, &()) {
                left.merge(&target, &());
                if let Some(right) = right {
                    if left.is_mergable(&right, &()) {
                        left.merge(&right, &());
                        Ok(())
                    } else {
                        return self.insert_at_pos(
                            Position::Start,
                            child_index + 1,
                            0,
                            right,
                            notify,
                        );
                    }
                } else {
                    Ok(())
                }
            } else if let Some(right) = right {
                if target.is_mergable(&right, &()) {
                    target.merge(&right, &());
                    return self.insert_at_pos(Position::Start, child_index + 1, 0, target, notify);
                } else {
                    let result =
                        self.insert_at_pos(Position::Start, child_index + 1, 0, target, notify);
                    if let Err(new) = result {
                        if self.children.len() >= child_index + 2 {
                            // insert one element should not cause Err
                            self.insert_at_pos(Position::Start, child_index + 2, 0, right, notify)
                                .unwrap();
                            return Err(new);
                        } else {
                            let new_insert_index = child_index + 2 - self.children.len();
                            // insert one element should not cause Err
                            new.as_leaf_mut()
                                .unwrap()
                                .insert_at_pos(Position::Start, new_insert_index, 0, right, notify)
                                .unwrap();
                            return Err(new);
                        }
                    } else {
                        return self.insert_at_pos(
                            Position::Start,
                            child_index + 2,
                            0,
                            right,
                            notify,
                        );
                    }
                }
            } else {
                return self.insert_at_pos(pos, child_index + 1, offset, target, notify);
            }
        } else {
            self.children[child_index] = self.bump.alloc(target);
            if let Some(right) = right {
                self.insert_at_pos(Position::Start, child_index + 1, 0, right, notify)
            } else {
                Ok(())
            }
        }
    }

    /// this is a effect-less operation, it will not modify the data, it returns the needed change at the given index instead
    pub(crate) fn pure_update<U>(
        &self,
        child_index: usize,
        offset: usize,
        len: usize,
        update_fn: &mut U,
    ) -> Option<Vec<T>>
    where
        U: FnMut(&mut T),
    {
        let mut ans = vec![];
        if len == 0 {
            return None;
        }

        let child = &self.children[child_index];
        if offset == 0 && child.content_len() == len {
            let mut element = (**child).clone();
            update_fn(&mut element);
            ans.push(element);
            return Some(ans);
        }

        if offset != 0 {
            ans.push(child.slice(0, offset));
        }
        let mut target = child.slice(offset, offset + len);
        update_fn(&mut target);
        if !ans.is_empty() {
            if ans[0].is_mergable(&target, &()) {
                ans[0].merge(&target, &());
            } else {
                ans.push(target);
            }
        } else {
            ans.push(target);
        }

        if offset + len < child.content_len() {
            let right = child.slice(offset + len, child.content_len());
            let mut merged = false;
            if let Some(last) = ans.last_mut() {
                if last.is_mergable(&right, &()) {
                    merged = true;
                    last.merge(&right, &());
                }
            }

            if !merged {
                ans.push(right);
            }
        }

        Some(ans)
    }

    pub(crate) fn apply_updates<F>(
        &mut self,
        mut updates: Vec<(usize, Vec<T>)>,
        notify: &mut F,
    ) -> Result<(), Vec<&'bump mut Node<'bump, T, A>>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        updates.sort_by_key(|x| x.0);
        let mut i = 0;
        let mut j = 1;
        // try merge sibling updates
        while i + j < updates.len() {
            if updates[i].0 + j == updates[i + j].0 {
                let (a, b) = arref::array_mut_ref!(&mut updates, [i, i + j]);
                for node in b.1.drain(..) {
                    a.1.push(node);
                }

                j += 1;
            } else {
                i += j;
                j = 1;
            }
        }

        let mut new_children: Vec<&mut T> = Vec::new();
        let mut self_children = std::mem::replace(&mut self.children, BumpVec::new_in(self.bump));
        let mut last_end = 0;
        // append element to the new_children list
        for (index, replace) in updates {
            for child in self_children.drain(0..index + 1 - last_end) {
                new_children.push(child);
            }

            new_children.pop();

            for element in replace {
                let mut merged = false;
                if let Some(last) = new_children.last_mut() {
                    if last.is_mergable(&element, &()) {
                        last.merge(&element, &());
                        merged = true;
                    }
                }
                if !merged {
                    new_children.push(self.bump.alloc(element));
                }
            }

            last_end = index + 1;
        }

        for child in self_children.drain(..) {
            new_children.push(child);
        }

        if new_children.len() <= A::MAX_CHILDREN_NUM {
            for child in new_children {
                notify(child, self);
                self.children.push(child);
            }

            A::update_cache_leaf(self);
            Ok(())
        } else {
            let children_nums =
                distribute(new_children.len(), A::MIN_CHILDREN_NUM, A::MAX_CHILDREN_NUM);
            let mut index = 0;
            for child in new_children.drain(..children_nums[index]) {
                notify(child, self);
                self.children.push(child);
            }

            index += 1;
            A::update_cache_leaf(self);
            let mut leaf_vec = Vec::new();
            while !new_children.is_empty() {
                let new_leaf_node = self
                    .bump
                    .alloc(Node::Leaf(LeafNode::new(self.bump, self.parent)));
                let new_leaf = new_leaf_node.as_leaf_mut().unwrap();
                for child in new_children.drain(..children_nums[index]) {
                    notify(child, new_leaf);
                    new_leaf.children.push(child);
                }

                index += 1;
                A::update_cache_leaf(new_leaf);
                leaf_vec.push(new_leaf_node);
            }

            let next = self.next;
            let mut last = self;
            for leaf in leaf_vec.iter_mut() {
                Self::connect(Some(last), Some(leaf.as_leaf_mut().unwrap()));
                last = leaf.as_leaf_mut().unwrap();
            }

            // SAFETY: there will not be shared mutable references
            Self::connect(Some(last), unsafe { next.map(|mut x| x.as_mut()) });
            Err(leaf_vec)
        }
    }

    fn with_cache_updated(
        &mut self,
        result: Result<(), &'bump mut Node<'bump, T, A>>,
    ) -> Result<(), &'bump mut Node<'bump, T, A>> {
        match result {
            Ok(_) => {
                A::update_cache_leaf(self);
                Ok(())
            }
            Err(new) => {
                A::update_cache_leaf(self);
                A::update_cache_leaf(new.as_leaf_mut().unwrap());
                Err(new)
            }
        }
    }

    fn _insert_at_pos<F>(
        &mut self,
        mut pos: Position,
        mut child_index: usize,
        mut offset: usize,
        value: T,
        notify: &mut F,
    ) -> Result<(), &'bump mut Node<'bump, T, A>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        let self_ptr = self as *mut _;
        let prev = {
            if (pos == Position::Start || pos == Position::Before) && child_index > 0 {
                Some(&mut self.children[child_index - 1])
            } else if pos == Position::After || pos == Position::End {
                child_index += 1;
                offset = 0;
                pos = Position::Start;
                Some(&mut self.children[child_index - 1])
            } else {
                None
            }
        };
        if let Some(prev) = prev {
            // clean cut, should no split
            if prev.is_mergable(&value, &()) {
                prev.merge(&value, &());
                notify(prev, self_ptr);
                return Ok(());
            }
        }
        let clean_cut = pos != Position::Middle;
        if clean_cut {
            return self._insert_with_split(child_index, value, notify);
        }
        // need to split child
        let a = self.children[child_index].slice(0, offset);
        let b = self.children[child_index].slice(offset, self.children[child_index].content_len());
        self.children[child_index] = self.bump.alloc(a);
        if self.children.len() >= A::MAX_CHILDREN_NUM - 1 {
            let next_node = self._split(notify);
            let next_leaf = next_node.as_leaf_mut().unwrap();
            if child_index < self.children.len() {
                notify(&value, self_ptr);
                notify(&b, self_ptr);
                self.children
                    .insert(child_index + 1, self.bump.alloc(value));
                self.children.insert(child_index + 2, self.bump.alloc(b));

                let last_child = self.children.pop().unwrap();
                notify(last_child, next_leaf);
                next_leaf.children.insert(0, last_child);
            } else {
                notify(&value, next_leaf);
                next_leaf.children.insert(
                    child_index - self.children.len() + 1,
                    self.bump.alloc(value),
                );
                notify(&b, next_leaf);
                next_leaf
                    .children
                    .insert(child_index - self.children.len() + 2, self.bump.alloc(b));
            }

            return Err(next_node);
        }
        notify(&b, self);
        notify(&value, self);
        self.children.insert(child_index + 1, self.bump.alloc(b));
        self.children
            .insert(child_index + 1, self.bump.alloc(value));
        Ok(())
    }

    #[inline]
    pub fn next(&self) -> Option<&Self> {
        // SAFETY: internal variant ensure prev and next are valid reference
        unsafe { self.next.map(|p| p.as_ref()) }
    }

    #[inline]
    pub fn next_mut(&mut self) -> Option<&mut Self> {
        // SAFETY: internal variant ensure prev and next are valid reference
        unsafe { self.next.map(|mut p| p.as_mut()) }
    }

    #[inline]
    pub fn prev(&self) -> Option<&Self> {
        // SAFETY: internal variant ensure prev and next are valid reference
        unsafe { self.prev.map(|p| p.as_ref()) }
    }

    #[inline]
    pub fn prev_mut(&mut self) -> Option<&mut Self> {
        // SAFETY: internal variant ensure prev and next are valid reference
        unsafe { self.prev.map(|mut p| p.as_mut()) }
    }

    #[inline]
    pub fn children(&self) -> &[&'bump mut T] {
        &self.children
    }
}

impl<'a, T: Rle, A: RleTreeTrait<T>> LeafNode<'a, T, A> {
    /// Delete may cause the children num increase, because splitting may happen
    ///
    pub(crate) fn delete<F>(
        &mut self,
        start: Option<A::Int>,
        end: Option<A::Int>,
        notify: &mut F,
    ) -> Result<(), &'a mut Node<'a, T, A>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        if self.children.is_empty() {
            return Ok(());
        }

        let (del_start, del_relative_from) = start.map_or((0, None), |x| self._delete_start(x));
        let (del_end, del_relative_to) =
            end.map_or((self.children.len(), None), |x| self._delete_end(x));
        let mut handled = false;
        let mut result = Ok(());
        if let (Some(del_relative_from), Some(del_relative_to)) =
            (del_relative_from, del_relative_to)
        {
            if del_start - 1 == del_end {
                let end = &mut self.children[del_end];
                let (left, right) = (
                    end.slice(0, del_relative_from),
                    end.slice(del_relative_to, end.content_len()),
                );

                *end = self.bump.alloc(left);
                result = self._insert_with_split(del_end + 1, right, notify);
                handled = true;
            }
        }

        if !handled {
            if let Some(del_relative_from) = del_relative_from {
                self.children[del_start - 1] = self
                    .bump
                    .alloc(self.children[del_start - 1].slice(0, del_relative_from));
            }
            if let Some(del_relative_to) = del_relative_to {
                let self_ptr = self as *mut _;
                let end = &mut self.children[del_end];
                *end = self
                    .bump
                    .alloc(end.slice(del_relative_to, end.content_len()));
                notify(end, self_ptr);
            }
        }

        if del_start < del_end {
            for _ in self.children.drain(del_start..del_end) {}
        }

        A::update_cache_leaf(self);
        if let Err(new) = &mut result {
            A::update_cache_leaf(new.as_leaf_mut().unwrap());
        }

        result
    }

    fn _insert_with_split<F>(
        &mut self,
        index: usize,
        value: T,
        notify: &mut F,
    ) -> Result<(), &'a mut Node<'a, T, A>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        if self.children.len() == A::MAX_CHILDREN_NUM {
            let ans = self._split(notify);
            if index <= self.children.len() {
                notify(&value, self);
                self.children.insert(index, self.bump.alloc(value));
            } else {
                let leaf = ans.as_leaf_mut().unwrap();
                notify(&value, leaf);
                leaf.children
                    .insert(index - self.children.len(), self.bump.alloc(value));
            }

            Err(ans)
        } else {
            notify(&value, self);
            self.children.insert(index, self.bump.alloc(value));
            Ok(())
        }
    }

    pub(crate) fn get_index_in_parent(&self) -> Option<usize> {
        let parent = self.parent;
        // SAFETY: we know parent must be valid
        let parent = unsafe { parent.as_ref() };
        parent
            .children
            .iter()
            .position(|child| std::ptr::eq(child.as_leaf().unwrap(), self))
    }
}

impl<'a, T: Rle, A: RleTreeTrait<T>> Debug for LeafNode<'a, T, A> {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        let mut debug_struct = f.debug_struct("LeafNode");
        debug_struct.field("children", &self.children);
        debug_struct.field("cache", &self.cache);
        debug_struct.field("children_num", &self.children.len());
        debug_struct.finish()
    }
}
